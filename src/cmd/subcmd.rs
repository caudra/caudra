use std::env;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};

use caudra_agent::AgentMode;
use caudra_agent::mcp::{McpSession, config as mcp_config, oauth as mcp_oauth};
use caudra_agent::prompt::profile::{PromptProfileCatalog, SystemPromptProfile};
use caudra_agent::template::{self, Vars};
use caudra_agent::tools::deferral::{deferred_names, push_unbound_catalog};
use caudra_agent::tools::native::skill::{self, SkillDirCandidate, SkillInventoryEntry};
use caudra_agent::tools::profile_policy::{
    CEILING_DISABLED, LEGACY_LOADING, PROFILE_DISABLED, PROFILE_LOADING, registered_decision,
};
use caudra_agent::tools::registry::ToolDefinitions;
use caudra_agent::tools::report::{CATALOG_SOURCE, REASON_CATALOG, REASON_CONFIG, REASON_DEFERRED};
use caudra_agent::tools::{
    BuiltinDeferral, DeferralSession, DescriptionContext, SHELL_TOOL_NAME, TOOL_SEARCH_TOOL_NAME,
    ToolAudience, ToolFilter, ToolRegistry, ToolState, builtin_report, execution, is_tool_enabled,
};
use caudra_config::providers::{
    Protocol, ProviderDef, ProvidersConfig, all_builtins, builtin_provider, custom_provider_slug,
    resolve_api_key_env, resolve_base_url, resolve_default_model, resolve_display_name,
    resolve_login_url, slugify,
};
use caudra_config::{
    AgentConfig, Config, DefaultEffect, ModelPolicy, PermissionsConfig, ProfileToolPolicy,
    ProfileToolSource, ToolKey,
};
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::{fetch_all_models, seed_setup_thinking};
use caudra_providers::{
    Model, ModelMarker, ModelPurpose, ProviderData, ThinkingConfig, Timeouts, catalog_providers,
};
use caudra_providers::{anthropic_auth, copilot_auth, dynamic, openai_auth, xai_auth};
use caudra_storage::StateDir;
use caudra_storage::auth::{
    MAX_WORKCELL_BEARER_TOKEN_BYTES, ProviderAuth, ProviderCredentials, WorkcellCredential,
    WorkcellCredentialName, delete_provider_credentials, delete_workcell_credential,
    list_workcell_credentials, load_provider_credentials, save_provider_credentials,
    save_workcell_credential, try_load_provider_auth,
};
use caudra_storage::model::persist_model_for_every_mode;
use caudra_storage::sessions::StoredMode;
use caudra_workspace::PlanRef;
use serde_json::Value;

use crate::cli::{AuthMethod, Cli, normalize_tool_name};
use crate::setup::resolve_model;

const PROMPT_PLAN_PATH: &str = "<active plan document>";
const PROMPT_PLAN_REFERENCE: &str = "active-plan-inspection";
const REASON_INSPECTION_UNAVAILABLE: &str = "unavailable for this inspection runtime or mode";
const AUTH_STATUS_EMPTY: &str = "       ";
const AUTH_STATUS_ENV: &str = "\x1b[33m~ env  \x1b[0m";
const AUTH_STATUS_KEY: &str = "\x1b[32m✓ key  \x1b[0m";
const AUTH_STATUS_OAUTH: &str = "\x1b[32m✓ oauth\x1b[0m";
const PROVIDER_SLUG_WIDTH: usize = 14;
const MODEL_COLUMN_GAP: &str = "  ";
const MODEL_JOB_HEADING: &str = "Job";
const MODEL_BINDING_HEADING: &str = "Binding";
const MODEL_RESOLVED_HEADING: &str = "Resolved";
const MODEL_DEFAULT_BINDING: &str = "default";
const MODEL_RESOLUTION_ERROR: &str = "error: ";
const SKILLS_HEADING: &str = "Skills";
const SKILL_DIRS_HEADING: &str = "Directories";
const NO_SKILLS_FOUND: &str = "No skills found.";
const SKILL_SCOPE_WIDTH: usize = 8;
const SKILL_DIR_STATE_WIDTH: usize = 11;

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

pub fn workcell_credential_set(
    name: &WorkcellCredentialName,
    from_stdin: bool,
    storage: &StateDir,
) -> Result<()> {
    let token = if from_stdin {
        read_workcell_bearer(io::stdin().lock()).context("read Workcell bearer token from stdin")?
    } else {
        rpassword::prompt_password("Workcell bearer token: ")
            .context("read hidden Workcell bearer token")?
    };
    let credential = WorkcellCredential::new(token).context("validate Workcell bearer token")?;
    save_workcell_credential(storage, name, &credential).context("save Workcell credential")?;
    println!("Saved Workcell credential '{name}'.");
    Ok(())
}

fn read_workcell_bearer(reader: impl Read) -> Result<String> {
    let limit = MAX_WORKCELL_BEARER_TOKEN_BYTES as u64 + 3;
    let mut token = String::new();
    reader.take(limit).read_to_string(&mut token)?;
    if token.len() > MAX_WORKCELL_BEARER_TOKEN_BYTES + 2 {
        bail!("Workcell bearer token exceeds {MAX_WORKCELL_BEARER_TOKEN_BYTES} bytes");
    }
    if token.ends_with('\n') {
        token.pop();
        if token.ends_with('\r') {
            token.pop();
        }
    }
    Ok(token)
}

pub fn workcell_credential_list(storage: &StateDir) -> Result<()> {
    let credentials = list_workcell_credentials(storage).context("list Workcell credentials")?;
    if credentials.is_empty() {
        println!("No Workcell credentials stored.");
        return Ok(());
    }
    for credential in credentials {
        println!("{}\t{}", credential.name, credential.updated_at_millis);
    }
    Ok(())
}

pub fn workcell_credential_delete(name: &WorkcellCredentialName, storage: &StateDir) -> Result<()> {
    if delete_workcell_credential(storage, name).context("delete Workcell credential")? {
        println!("Deleted Workcell credential '{name}'.");
    } else {
        println!("Workcell credential '{name}' was not stored.");
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
        seed_setup_thinking(storage, model);
        persist_model_for_every_mode(storage, model);
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
    let slug = custom_provider_slug(&name)?;

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

fn model_marker_label(marker: ModelMarker) -> &'static str {
    match marker {
        ModelMarker::Small => "Small",
        ModelMarker::Fast => "Fast",
        ModelMarker::Best => "Best",
    }
}

/// Specs stay first, and a batch with no supply markers stays unpadded, so
/// piping `caudra models` into another command keeps working unchanged.
///
/// Width is per batch because batches stream in as each provider answers, and
/// buffering every provider to align one column would hold back the output.
fn model_lines(specs: &[String]) -> Vec<String> {
    let markers: Vec<Option<ModelMarker>> = specs
        .iter()
        .map(|spec| {
            spec.split_once('/')
                .and_then(|(provider, id)| Model::marker_of(provider, id))
        })
        .collect();
    let width = specs
        .iter()
        .zip(&markers)
        .filter(|(_, marker)| marker.is_some())
        .map(|(spec, _)| spec.chars().count())
        .max();
    specs
        .iter()
        .zip(markers)
        .map(|(spec, marker)| match (marker, width) {
            (Some(marker), Some(width)) => {
                format!(
                    "{spec:<width$}{MODEL_COLUMN_GAP}{}",
                    model_marker_label(marker)
                )
            }
            _ => spec.clone(),
        })
        .collect()
}

struct ModelJobRow {
    job: &'static str,
    binding: String,
    resolved: String,
}

fn model_job_rows(anchor: &Model, policy: &ModelPolicy) -> Vec<ModelJobRow> {
    ModelPurpose::ALL
        .into_iter()
        .map(|purpose| {
            let binding = model_registry::binding(purpose);
            model_job_row(purpose, binding, anchor, policy)
        })
        .collect()
}

fn model_job_row(
    purpose: ModelPurpose,
    binding: Option<Binding>,
    anchor: &Model,
    policy: &ModelPolicy,
) -> ModelJobRow {
    let resolved = Model::resolve_binding_if_available(purpose, binding.as_ref(), anchor, policy)
        .map(|model| model.spec())
        .unwrap_or_else(|error| format!("{MODEL_RESOLUTION_ERROR}{error}"));
    ModelJobRow {
        job: purpose.label(),
        binding: binding
            .as_ref()
            .map_or_else(|| MODEL_DEFAULT_BINDING.to_owned(), ToString::to_string),
        resolved,
    }
}

fn render_model_jobs(rows: &[ModelJobRow]) -> String {
    let job_width = rows
        .iter()
        .map(|row| row.job.chars().count())
        .chain([MODEL_JOB_HEADING.len()])
        .max()
        .unwrap_or_default();
    let binding_width = rows
        .iter()
        .map(|row| row.binding.chars().count())
        .chain([MODEL_BINDING_HEADING.len()])
        .max()
        .unwrap_or_default();
    let mut output = String::new();
    let _ = writeln!(
        output,
        "{MODEL_JOB_HEADING:<job_width$}{MODEL_COLUMN_GAP}{MODEL_BINDING_HEADING:<binding_width$}{MODEL_COLUMN_GAP}{MODEL_RESOLVED_HEADING}"
    );
    for row in rows {
        let _ = writeln!(
            output,
            "{:<job_width$}{MODEL_COLUMN_GAP}{:<binding_width$}{MODEL_COLUMN_GAP}{}",
            row.job, row.binding, row.resolved
        );
    }
    output
}

fn print_model_jobs(model_arg: Option<&str>, config: &Config) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    model_registry::load_from_storage(&storage).context("load model purpose bindings")?;
    let anchor = resolve_model(model_arg, &config.provider, &storage, StoredMode::Build)?;
    print!(
        "{}",
        render_model_jobs(&model_job_rows(&anchor, &config.provider.model_policy))
    );
    Ok(())
}

pub fn models(cli: &Cli, jobs: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let storage = StateDir::resolve().context("resolve data directory")?;
    let local = crate::cli::WorkcellSelectorArgs::default();
    let selection = if cli.workcell.sandbox.is_some() {
        &local
    } else {
        &cli.workcell
    };
    let runtime = super::workcell_runtime::WorkcellRuntime::initialize(
        selection,
        &cwd,
        &storage,
        ToolRegistry::global(),
        cli.startup.features,
    )?;

    let host = super::cli_plugin_host(cli, Arc::clone(ToolRegistry::global_arc()))?;
    let config = super::load_settings(&host, &cli.startup, &cwd, runtime.is_remote())?
        .into_config(false)
        .context("invalid config")?;
    if jobs {
        return print_model_jobs(cli.model.as_deref(), &config);
    }

    smol::block_on(fetch_all_models(
        &config.provider.model_policy,
        |batch| {
            for line in model_lines(&batch.models) {
                println!("{line}");
            }
            for warning in batch.warnings {
                eprintln!("warning: {warning}");
            }
        },
        None,
    ));
    Ok(())
}

pub fn index(cli: &Cli, path: &str) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let storage = StateDir::resolve().context("resolve data directory")?;
    let runtime = super::workcell_runtime::WorkcellRuntime::initialize(
        &cli.workcell,
        &cwd,
        &storage,
        ToolRegistry::global(),
        cli.startup.features,
    )?;

    let mut host = super::cli_plugin_host(cli, Arc::clone(ToolRegistry::global_arc()))?;

    let config = super::load_config(&host, cli, &cwd, runtime.is_remote())?;
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&host.plugin_rules(), &cwd);

    host.load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    ensure_index_enabled(&config.agent)?;
    let reg = ToolRegistry::global_arc();
    let project_cwd = if runtime.is_remote() {
        Path::new(&runtime.display().cwd)
    } else {
        &cwd
    };
    let mut ctx = caudra_agent::tools::cli_tool_ctx(project_cwd);
    ctx.workspace_session = runtime.workspace_session().cloned();
    ctx.remote_project_context = runtime.remote_project_context().cloned();
    ctx.permissions = Arc::new(
        caudra_agent::permissions::PermissionManager::new_persistent(
            config.permissions.clone(),
            project_cwd.to_path_buf(),
            host.plugin_rules(),
        ),
    );
    ctx.permissions.replace_remote_permission_asset(
        runtime
            .remote_project_context()
            .and_then(|context| context.permissions()),
    )?;
    ctx.config = config.agent;
    print!("{}", execute_index(reg, path, ctx)?);
    Ok(())
}

pub fn remote_control(cli: &Cli, args: &[String]) -> Result<()> {
    let args = args.join(" ");
    caudra_workspace::WorkspaceControlCommand::parse(&args)
        .map_err(color_eyre::eyre::Error::msg)?;
    if !cli.workcell.is_set() {
        bail!("Select a remote Workcell profile or endpoint for remote control");
    }
    let storage = StateDir::resolve().context("resolve data directory")?;
    let workspace =
        super::workcell_runtime::connect_control(&cli.workcell, &storage, cli.startup.features)?;
    let output = smol::block_on(caudra_workspace::execute_workspace_control(
        &workspace.workspace,
        &args,
    ))
    .map_err(color_eyre::eyre::Error::msg)?;
    println!("{output}");
    Ok(())
}

fn ensure_index_enabled(config: &caudra_config::AgentConfig) -> Result<()> {
    if !is_tool_enabled(&config.disabled_tools, "file_index") {
        bail!("index is disabled by plugins.index.enabled = false");
    }
    Ok(())
}

fn execute_index(
    registry: &ToolRegistry,
    path: &str,
    ctx: caudra_agent::tools::ToolContext,
) -> Result<String> {
    let input = serde_json::json!({"path": path});
    let result = smol::block_on(caudra_agent::agent::tool_dispatch::run(
        registry,
        None,
        "cli-index".into(),
        "file_index",
        &input,
        &ctx,
        caudra_agent::agent::tool_dispatch::Emit::Silent,
    ));
    if result.is_error {
        bail!("index failed: {}", result.output.as_text());
    }
    Ok(result.output.as_text())
}

pub fn mcp_auth(server: &str, storage: &StateDir, global_only: bool) -> Result<()> {
    smol::block_on(async {
        let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
        let (config, _) = if global_only {
            mcp_config::load_global_config(&cwd)
        } else {
            mcp_config::load_config(&cwd)
        };
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
/// Padded to a common width so the name column starts at one column.
const STATE_WIDTH: usize = 4;

#[derive(serde::Serialize)]
struct ToolRow {
    name: String,
    source: String,
    state: ToolState,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission: Option<&'static str>,
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
    definitions: &ToolDefinitions,
    policy: &ProfileToolPolicy,
) -> Vec<ToolRow> {
    let deferral = BuiltinDeferral::resolve(&config.agent, model);
    let ctx = DescriptionContext {
        filter,
        audience: ToolAudience::MAIN,
        workflows_available: false,
    };
    let mut rows: Vec<ToolRow> = registry
        .iter()
        .iter()
        .map(|entry| {
            let name = entry.name();
            let keys = match name {
                SHELL_TOOL_NAME => vec![ToolKey::native(name), ToolKey::native(LEGACY_SHELL_KEY)],
                _ => vec![ToolKey::native(name)],
            };
            let mut report =
                builtin_report(name, filter, cli_disallowed, &config.agent, model, deferral);
            let decision = registered_decision(
                entry,
                &ctx,
                policy,
                &AgentMode::Build,
                report.state == ToolState::Lazy,
            );
            if !matches!(decision.reason, CEILING_DISABLED | LEGACY_LOADING) {
                report.reason = Some(decision.reason);
            }
            let state = inspection_tool_state(definitions, name);
            if state != report.state {
                if state == ToolState::Off && decision.available() {
                    report.reason = Some(REASON_INSPECTION_UNAVAILABLE);
                }
                report.state = state;
            }
            ToolRow {
                name: name.to_owned(),
                source: entry.source.as_log_field().into_owned(),
                state: report.state,
                note: report.reason,
                permission: permission_default(&config.permissions, &keys),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Derived, not looked up: `tool_search` is synthesized into the request
/// array whenever something is deferred, so a lazy row anywhere means the
/// model is offered the catalog too.
fn catalog_row(builtin: &[ToolRow], mcp: &[ToolRow]) -> Option<ToolRow> {
    if builtin
        .iter()
        .chain(mcp)
        .any(|row| row.name == TOOL_SEARCH_TOOL_NAME && row.state.reaches_model())
    {
        return None;
    }
    let lazy = builtin
        .iter()
        .chain(mcp)
        .any(|row| row.state == ToolState::Lazy);
    lazy.then(|| ToolRow {
        name: TOOL_SEARCH_TOOL_NAME.to_owned(),
        source: CATALOG_SOURCE.to_owned(),
        state: ToolState::On,
        note: Some(REASON_CATALOG),
        permission: None,
    })
}

fn mcp_rows(mcp: Option<&McpSession>, config: &Config, policy: &ProfileToolPolicy) -> Vec<ToolRow> {
    let Some(mcp) = mcp else {
        return Vec::new();
    };
    let mut rows: Vec<ToolRow> =
        mcp.request_snapshot()
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
                let (state, mut note) = match (tool.disabled, tool.deferred) {
                    (true, _) => (ToolState::Off, Some(REASON_INSPECTION_UNAVAILABLE)),
                    (false, true) => (ToolState::Lazy, Some(REASON_DEFERRED)),
                    _ => (ToolState::On, None),
                };
                if config.agent.disabled_tools.iter().any(|pattern| {
                    caudra_config::tool_pattern_matches(pattern, &tool.qualified_name)
                }) {
                    note = Some(REASON_CONFIG);
                } else if policy
                    .exposure(&tool.qualified_name, ProfileToolSource::Mcp)
                    .is_some()
                {
                    note = Some(if tool.disabled {
                        PROFILE_DISABLED
                    } else {
                        PROFILE_LOADING
                    });
                }
                ToolRow {
                    state,
                    note,
                    permission: permission_default(&config.permissions, &keys),
                    source: format!("{SOURCE_MCP}:{}", tool.server),
                    name: tool.wire_name,
                }
            })
            .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

fn inspection_definitions(
    registry: &ToolRegistry,
    vars: &Vars,
    ctx: &DescriptionContext,
    config: &AgentConfig,
    model: &Model,
    policy: &ProfileToolPolicy,
    mode: &AgentMode,
) -> Result<ToolDefinitions> {
    policy
        .validate_bindings(registry.iter().iter().map(|entry| entry.name()))
        .map_err(|error| eyre!(error))
        .context("resolve system prompt profile tools")?;
    let deferred = deferred_names(
        &config.allowed_tools,
        BuiltinDeferral::resolve(config, model),
    );
    let mut definitions = registry.definitions_split_with_policy(
        vars,
        ctx,
        model.supports_tool_examples(),
        &deferred,
        policy,
        mode,
    );
    execution::configure_tools(
        &mut definitions.declared,
        &mut definitions.deferred,
        config,
        ctx.audience == ToolAudience::MAIN,
        true,
    );
    Ok(definitions)
}

fn inspection_vars(
    vars: Vars,
    profiles: &PromptProfileCatalog,
    config: &Config,
    model: &Model,
) -> Vars {
    let thinking = config
        .always_thinking
        .clone()
        .map(ThinkingConfig::from)
        .unwrap_or_default();
    let bindings = profiles.bind_for_tasks(
        model,
        model,
        &thinking,
        &config.provider.model_policy,
        Timeouts::default(),
    );
    vars.set(
        "{task_system_prompt_profiles}",
        bindings.task_tool_summary("Caudra's built-in task prompt"),
    )
}

fn inspection_tool_state(definitions: &ToolDefinitions, name: &str) -> ToolState {
    if definitions
        .declared
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"].as_str() == Some(name)))
    {
        ToolState::On
    } else if definitions
        .deferred
        .iter()
        .any(|tool| tool.name.as_ref() == name)
    {
        ToolState::Lazy
    } else {
        ToolState::Off
    }
}

fn inspection_catalog(mut definitions: ToolDefinitions, mcp: Option<&McpSession>) -> Value {
    let deferred = DeferralSession::new(definitions.deferred, std::iter::empty());
    let mut sections = Vec::new();
    sections.extend(
        deferred
            .request_snapshot()
            .extend_declared(&mut definitions.declared),
    );
    if let Some(mcp) = mcp {
        sections.extend(
            mcp.request_snapshot()
                .extend_declared(&mut definitions.declared),
        );
    }
    push_unbound_catalog(
        &mut definitions.declared,
        &sections,
        ToolRegistry::global().has(caudra_agent::tools::TOOL_SEARCH_TOOL_NAME),
    );
    definitions.declared
}

fn inspection_mcp(
    cwd: &Path,
    remote: bool,
    config: &AgentConfig,
    policy: Arc<ProfileToolPolicy>,
    mode: &AgentMode,
) -> Option<McpSession> {
    let (handle, errors) = if remote {
        smol::block_on(caudra_agent::mcp::start_global_connected(cwd))
    } else {
        smol::block_on(caudra_agent::mcp::start_connected(cwd))
    };
    if !errors.is_empty() {
        eprintln!("warning: {errors}");
    }
    let ceiling = ToolFilter::All.for_mode(if mode.is_planning() {
        &AgentMode::ReadOnly
    } else {
        mode
    });
    handle.map(|handle| {
        McpSession::new(handle, &[])
            .with_disabled_tools(&config.disabled_tools)
            .with_profile_policy(policy, ceiling)
    })
}

fn print_group(heading: &str, rows: &[ToolRow]) {
    if rows.is_empty() {
        return;
    }
    let name_width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);
    let source_width = rows.iter().map(|row| row.source.len()).max().unwrap_or(0);
    println!("{heading}");
    for row in rows {
        let state = row.state.label();
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
            "  {state:<STATE_WIDTH$}  {:name_width$}  {:source_width$}{detail}",
            row.name, row.source
        );
        println!("{}", line.trim_end());
    }
    println!();
}

pub fn tools(cli: &Cli, enabled_only: bool, json: bool, names: bool, schemas: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let storage = StateDir::resolve().context("resolve data directory")?;
    let runtime = super::workcell_runtime::WorkcellRuntime::initialize(
        &cli.workcell,
        &cwd,
        &storage,
        ToolRegistry::global(),
        cli.startup.features,
    )?;

    let reg = ToolRegistry::global_arc();
    let mut host = super::cli_plugin_host(cli, Arc::clone(reg))?;
    let config = super::load_config(&host, cli, &cwd, runtime.is_remote())?;
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&host.plugin_rules(), &cwd);
    host.load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let mut model = resolve_model(
        cli.model.as_deref(),
        &config.provider,
        &storage,
        StoredMode::Build,
    )?;
    caudra_providers::provider::adjust_model(&mut model, Timeouts::default())?;
    let profiles = PromptProfileCatalog::discover_user();
    let profile = profiles
        .resolve(
            cli.system_prompt_profile
                .as_deref()
                .or(config.agent.system_prompt_profile.as_deref()),
        )
        .context("resolve system prompt profile")?;
    let policy = Arc::new(
        profile
            .as_deref()
            .map(SystemPromptProfile::tools)
            .cloned()
            .unwrap_or_default(),
    );
    let filter = ToolFilter::from_config(&config.agent, &model, &[]);
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflows_available: false,
    };
    let vars = if runtime.is_remote() {
        template::env_vars()
            .set("{cwd}", runtime.display().cwd.clone())
            .set("{platform}", runtime.display().platform.clone())
    } else {
        template::env_vars()
    };
    let vars = inspection_vars(vars, &profiles, &config, &model);
    let definitions = inspection_definitions(
        reg,
        &vars,
        &ctx,
        &config.agent,
        &model,
        &policy,
        &AgentMode::Build,
    )?;
    let mcp = inspection_mcp(
        &cwd,
        runtime.is_remote(),
        &config.agent,
        Arc::clone(&policy),
        &AgentMode::Build,
    );

    if schemas {
        let defs = inspection_catalog(definitions, mcp.as_ref());
        println!("{}", serde_json::to_string_pretty(&defs)?);
        return Ok(());
    }

    let cli_disallowed = cli
        .disallowed_tools
        .iter()
        .map(|tool| normalize_tool_name(tool))
        .collect::<Result<Vec<_>>>()?;
    let mut builtin = builtin_rows(
        reg,
        &filter,
        &config,
        &cli_disallowed,
        &model,
        &definitions,
        &policy,
    );
    let mut mcp_tools = mcp_rows(mcp.as_ref(), &config, &policy);
    if let Some(catalog) = catalog_row(&builtin, &mcp_tools) {
        let at = builtin.partition_point(|row| row.name < catalog.name);
        builtin.insert(at, catalog);
    }
    if enabled_only {
        builtin.retain(|row| row.state.reaches_model());
        mcp_tools.retain(|row| row.state.reaches_model());
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

/// Skills need neither a model nor MCP, so this stops short of both. The
/// built-in plugin-dev skill still has to be installed, or the listing would
/// disagree with the one the model sees.
pub fn skills(cli: &Cli, name: Option<&str>, names: bool, json: bool, dirs: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let storage = StateDir::resolve().context("resolve data directory")?;
    let runtime = super::workcell_runtime::WorkcellRuntime::initialize(
        &cli.workcell,
        &cwd,
        &storage,
        ToolRegistry::global(),
        cli.startup.features,
    )?;

    let reg = ToolRegistry::global_arc();
    let mut host = super::cli_plugin_host(cli, Arc::clone(reg))?;
    let config = super::load_config(&host, cli, &cwd, runtime.is_remote())?;
    super::configure_native_tools(&config.agent);
    host.load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    if let Some(name) = name {
        match skill::load(reg, name) {
            Ok(body) => println!("{body}"),
            Err(message) => bail!(message),
        }
        return Ok(());
    }

    let found = skill::inventory(reg);
    let candidates = skill::directories(reg);

    if names {
        for entry in &found {
            println!("{}", entry.name);
        }
    } else if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "skills": found
                    .iter()
                    .map(|entry| serde_json::json!({
                        "name": entry.name,
                        "description": entry.description,
                        "location": entry.location,
                        "scope": entry.scope.label(),
                    }))
                    .collect::<Vec<_>>(),
                "directories": candidates
                    .iter()
                    .map(|dir| serde_json::json!({
                        "path": dir.path,
                        "scope": dir.scope.label(),
                        "state": dir.state.label(),
                    }))
                    .collect::<Vec<_>>(),
            }))?
        );
    } else if dirs {
        print_skill_dirs(&candidates);
    } else {
        print_skills(&found);
    }
    Ok(())
}

fn print_skills(found: &[SkillInventoryEntry]) {
    println!("{SKILLS_HEADING}");
    if found.is_empty() {
        println!("  {NO_SKILLS_FOUND}");
        return;
    }
    let name_width = found
        .iter()
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or(0);
    for entry in found {
        println!(
            "  {:<SKILL_SCOPE_WIDTH$}{:<name_width$}  {}",
            entry.scope.label(),
            entry.name,
            entry.location
        );
        if !entry.description.is_empty() {
            println!("  {:<SKILL_SCOPE_WIDTH$}{}", "", entry.description);
        }
    }
}

fn print_skill_dirs(candidates: &[SkillDirCandidate]) {
    println!("{SKILL_DIRS_HEADING}");
    for dir in candidates {
        println!(
            "  {:<SKILL_DIR_STATE_WIDTH$}{:<SKILL_SCOPE_WIDTH$}{}",
            dir.state.label(),
            dir.scope.label(),
            dir.path.display()
        );
    }
}

pub fn prompt(
    cli: &Cli,
    variant: &crate::cli::PromptVariant,
    plan: bool,
    tools: bool,
    names: bool,
) -> Result<()> {
    use crate::cli::PromptVariant;
    use caudra_agent::agent::{build_system_prompt, environment_block, load_instruction_text};
    use caudra_agent::prompt::{
        PromptId, TASK_BUILD_CONTRACT, TASK_PLAN_CONTRACT, assemble_task_with_filter,
        plan_mode_prompt,
    };

    if plan && !matches!(variant, PromptVariant::System) {
        bail!("--plan can only be used with the 'system' prompt variant");
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let storage = StateDir::resolve().context("resolve data directory")?;
    let runtime = super::workcell_runtime::WorkcellRuntime::initialize(
        &cli.workcell,
        &cwd,
        &storage,
        ToolRegistry::global(),
        cli.startup.features,
    )?;

    let vars = if runtime.is_remote() {
        template::env_vars()
            .set("{cwd}", runtime.display().cwd.clone())
            .set("{platform}", runtime.display().platform.clone())
    } else {
        template::env_vars()
    };
    let reg = ToolRegistry::global_arc();
    let mut host = super::cli_plugin_host(cli, Arc::clone(reg))?;
    let config = super::load_config(&host, cli, &cwd, runtime.is_remote())?;
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&host.plugin_rules(), &cwd);
    host.load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let cwd_str = cwd.to_string_lossy();
    let instructions = if let Some(context) = runtime.remote_project_context() {
        caudra_agent::agent::load_remote_instructions(context, Some(&cwd)).text
    } else {
        load_instruction_text(&cwd_str)
    };
    let slots = host.event_handle().collect_prompt_slots(&config.agent);
    let prompt_profiles = PromptProfileCatalog::discover_user();
    let profile_name = cli
        .system_prompt_profile
        .as_deref()
        .or(config.agent.system_prompt_profile.as_deref());
    let system_prompt_profile = prompt_profiles
        .resolve(profile_name)
        .context("resolve system prompt profile")?;
    let mut model = crate::setup::resolve_model(
        cli.model.as_deref(),
        &config.provider,
        &storage,
        StoredMode::Build,
    )?;
    caudra_providers::provider::adjust_model(&mut model, caudra_providers::Timeouts::default())?;
    let policy = Arc::new(
        system_prompt_profile
            .as_deref()
            .map(SystemPromptProfile::tools)
            .cloned()
            .unwrap_or_default(),
    );
    let (audience, mode) = match variant {
        PromptVariant::System if plan && runtime.is_remote() => (
            ToolAudience::MAIN,
            AgentMode::RemotePlan(PlanRef::new(PROMPT_PLAN_REFERENCE)?),
        ),
        PromptVariant::System if plan => {
            (ToolAudience::MAIN, AgentMode::Plan(PROMPT_PLAN_PATH.into()))
        }
        PromptVariant::System => (ToolAudience::MAIN, AgentMode::Build),
        PromptVariant::Research => (ToolAudience::RESEARCH_SUB, AgentMode::ReadOnly),
        PromptVariant::General => (ToolAudience::GENERAL_SUB, AgentMode::Build),
    };
    let filter = ToolFilter::from_config(&config.agent, &model, &[]).for_mode(&mode);
    let vars = inspection_vars(vars, &prompt_profiles, &config, &model);
    let ctx = DescriptionContext {
        filter: &filter,
        audience,
        workflows_available: false,
    };
    let definitions =
        inspection_definitions(reg, &vars, &ctx, &config.agent, &model, &policy, &mode)?;
    if tools {
        let mcp = inspection_mcp(
            &cwd,
            runtime.is_remote(),
            &config.agent,
            Arc::clone(&policy),
            &mode,
        );
        let defs = inspection_catalog(definitions, mcp.as_ref());
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

    let slots = execution::execution_slots(
        &slots,
        &config.agent,
        audience == ToolAudience::MAIN,
        true,
        &definitions.declared,
        &definitions.deferred,
    );
    let filter = if policy.is_legacy() {
        filter
    } else {
        ToolFilter::Only(
            reg.iter()
                .iter()
                .filter(|entry| inspection_tool_state(&definitions, entry.name()).reaches_model())
                .map(|entry| entry.name().to_owned())
                .collect(),
        )
        .for_mode(&mode)
    };
    let output = match variant {
        PromptVariant::System => {
            let system = build_system_prompt(
                &instructions,
                &slots,
                &filter,
                system_prompt_profile.as_deref(),
            );
            let system = format!("{system}\n\n{}", environment_block(&vars, &model));
            if let Some(reminder) = plan_mode_prompt(&mode, |name| {
                inspection_tool_state(&definitions, name).reaches_model()
            }) {
                format!("{system}\n\n{reminder}")
            } else {
                system
            }
        }
        PromptVariant::Research => with_announcements(
            &assemble_task_with_filter(
                PromptId::Research,
                &slots,
                &filter,
                &instructions,
                system_prompt_profile.as_deref(),
            ),
            &vars,
            &model,
            TASK_PLAN_CONTRACT,
        ),
        PromptVariant::General => with_announcements(
            &assemble_task_with_filter(
                PromptId::General,
                &slots,
                &filter,
                &instructions,
                system_prompt_profile.as_deref(),
            ),
            &vars,
            &model,
            TASK_BUILD_CONTRACT,
        ),
    };

    print!("{output}");
    Ok(())
}

/// A task prompt carries neither its environment nor its mode: both are
/// announced to the subagent, so printing one without them would misrepresent
/// what the subagent is told.
fn with_announcements(
    prompt: &str,
    vars: &caudra_agent::template::Vars,
    model: &caudra_providers::Model,
    contract: &str,
) -> String {
    format!(
        "{}\n\n{}\n\n{contract}",
        vars.apply(prompt),
        caudra_agent::agent::environment_block(vars, model)
    )
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use caudra_agent::permissions::PermissionManager;
    use caudra_agent::tools::cli_tool_ctx;
    use caudra_agent::tools::profile_policy::{PLAN_MODE_REQUIRED, PLAN_TOOL_NAME};
    use caudra_config::{Effect, ExecutionMode, FeatureFlags, PermissionRule, RawConfig};
    use caudra_workcell::WorkcellHost;
    use tempfile::TempDir;
    use test_case::test_case;

    #[test_case(Effect::Allow; "authorized")]
    #[test_case(Effect::Deny; "denied")]
    fn index_subcommand_uses_permission_dispatch(effect: Effect) {
        const SOURCE: &str = "pub fn indexed() {}";
        const FILE: &str = "source.rs";
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join(FILE), SOURCE).unwrap();
        let host = WorkcellHost::new(root.path(), None).unwrap();
        let registry = ToolRegistry::new();
        host.register(&registry).unwrap();
        let mut ctx = cli_tool_ctx(root.path());
        ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: ToolKey::native("file_index"),
                    scope: Some("*".into()),
                    effect,
                }],
                ..PermissionsConfig::default()
            },
            root.path().to_path_buf(),
            Arc::default(),
        ));
        let result = execute_index(&registry, FILE, ctx);
        if effect == Effect::Allow {
            assert!(result.unwrap().contains("indexed"));
        } else {
            assert!(result.is_err());
        }
    }

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

    #[test]
    fn workcell_stdin_token_strips_only_the_line_ending() {
        const TOKEN: &str = "bearer-private-value";

        assert_eq!(
            read_workcell_bearer(format!("{TOKEN}\r\n").as_bytes()).unwrap(),
            TOKEN
        );
    }

    #[test]
    fn workcell_stdin_token_is_bounded_before_storage() {
        let input = "x".repeat(MAX_WORKCELL_BEARER_TOKEN_BYTES + 3);

        assert!(read_workcell_bearer(input.as_bytes()).is_err());
    }

    const FAST_SPEC: &str = "anthropic/claude-haiku-4-5";
    const SMALL_SPEC: &str = "openai/gpt-5.4-nano";
    const BEST_SPEC: &str = "openai/gpt-5.6-sol";
    const UNCURATED_SPEC: &str = "ollama/llama3";
    const AGGREGATED_SPEC: &str = "openrouter/anthropic/claude-haiku-4-5";
    const UNAVAILABLE_RESOLUTION: &str = "error: unavailable";
    const INSPECTION_TODO: &str = "todo_write";
    const INSPECTION_TASK: &str = "task";
    const INSPECTION_UNKNOWN: &str = "unregistered_inspection_tool";
    const INSPECTION_BINDING_ERROR: &str = "no registered binding";

    #[test_case(FAST_SPEC, ModelMarker::Fast ; "fast_default")]
    #[test_case(SMALL_SPEC, ModelMarker::Small ; "small_non_default")]
    #[test_case(BEST_SPEC, ModelMarker::Best ; "best_default")]
    fn a_listed_model_carries_its_supply_marker(spec: &str, marker: ModelMarker) {
        let lines = model_lines(&[spec.into()]);
        assert_eq!(
            lines,
            vec![format!(
                "{spec}{MODEL_COLUMN_GAP}{}",
                model_marker_label(marker)
            )]
        );
    }

    /// An aggregator has no catalogue of its own, so the marker has to come
    /// from the vendor named in the model id.
    #[test]
    fn an_aggregated_model_borrows_the_upstream_marker() {
        let lines = model_lines(&[AGGREGATED_SPEC.into()]);
        assert!(
            lines[0].ends_with(model_marker_label(ModelMarker::Fast)),
            "{lines:?}"
        );
    }

    /// A batch with no markers must pipe exactly as it did before the column
    /// existed, with no trailing padding.
    #[test]
    fn an_unmarked_batch_stays_bare() {
        assert_eq!(
            model_lines(&[UNCURATED_SPEC.into()]),
            vec![UNCURATED_SPEC.to_string()]
        );
    }

    #[test]
    fn a_mixed_batch_aligns_on_the_widest_marked_spec() {
        let lines = model_lines(&[FAST_SPEC.into(), UNCURATED_SPEC.into()]);
        assert_eq!(lines[1], UNCURATED_SPEC, "unmarked rows stay bare");
        assert!(lines[0].starts_with(FAST_SPEC));
    }

    #[test]
    fn model_jobs_renderer_aligns_plain_text_and_keeps_rows_after_errors() {
        let rows = [
            ModelJobRow {
                job: "Chat",
                binding: MODEL_DEFAULT_BINDING.into(),
                resolved: "provider/chat".into(),
            },
            ModelJobRow {
                job: "Subagent",
                binding: "same as fast".into(),
                resolved: UNAVAILABLE_RESOLUTION.into(),
            },
            ModelJobRow {
                job: "Title",
                binding: MODEL_DEFAULT_BINDING.into(),
                resolved: "provider/title".into(),
            },
        ];

        let rendered = render_model_jobs(&rows);
        let expected = format!(
            "Job       Binding       Resolved\n\
Chat      default       provider/chat\n\
Subagent  same as fast  {UNAVAILABLE_RESOLUTION}\n\
Title     default       provider/title\n"
        );

        assert_eq!(rendered, expected);
        assert!(!rendered.contains('\x1b'));
    }

    #[test]
    fn model_job_resolution_keeps_policy_errors_visible() {
        let anchor = Model::from_spec(FAST_SPEC).unwrap();
        let policy = ModelPolicy::new(&[], &[BEST_SPEC.to_string()]).unwrap();
        let row = model_job_row(
            ModelPurpose::Goal,
            Some(Binding::Exact(BEST_SPEC.into())),
            &anchor,
            &policy,
        );

        assert_eq!(row.binding, BEST_SPEC);
        assert!(row.resolved.starts_with(MODEL_RESOLUTION_ERROR));
        assert!(row.resolved.contains("not allowed"));
    }

    #[test_case("1", Some(Protocol::Openai) ; "chat_by_number")]
    #[test_case("openai", Some(Protocol::Openai) ; "chat_by_name")]
    #[test_case("2", Some(Protocol::OpenaiResponses) ; "responses_by_number")]
    #[test_case("openai-responses", Some(Protocol::OpenaiResponses) ; "responses_by_name")]
    #[test_case("unknown", None ; "unknown_protocol")]
    fn custom_protocol_choices(input: &str, expected: Option<Protocol>) {
        assert_eq!(custom_protocol(input), expected);
    }

    fn state_rows(states: &[ToolState]) -> Vec<ToolRow> {
        states
            .iter()
            .map(|state| ToolRow {
                name: String::new(),
                source: String::new(),
                state: *state,
                note: None,
                permission: None,
            })
            .collect()
    }

    #[test_case("eager", false, ToolState::On; "eager")]
    #[test_case("lazy", false, ToolState::Lazy; "explicitly_lazy")]
    #[test_case("disabled", false, ToolState::Off; "disabled")]
    #[test_case("eager", true, ToolState::Off; "global_disable_wins")]
    fn inspection_profiles_match_schema_catalog_and_report(
        exposure: &str,
        disabled: bool,
        expected: ToolState,
    ) {
        let registry = ToolRegistry::new();
        caudra_agent::tools::native::register(&registry, FeatureFlags::default()).unwrap();
        let mut config = RawConfig::default().into_config(false).unwrap();
        if disabled {
            config.agent.disabled_tools.push(INSPECTION_TODO.into());
        }
        let model = Model::from_spec(BEST_SPEC).unwrap();
        let filter = ToolFilter::from_config(&config.agent, &model, &[]);
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "default": "disabled", "overrides": {INSPECTION_TODO: exposure}
        }))
        .unwrap();
        let definitions = inspection_definitions(
            &registry,
            &Vars::new(),
            &DescriptionContext {
                filter: &filter,
                audience: ToolAudience::MAIN,
                workflows_available: false,
            },
            &config.agent,
            &model,
            &policy,
            &AgentMode::Build,
        )
        .unwrap();
        let rows = builtin_rows(
            &registry,
            &filter,
            &config,
            &[],
            &model,
            &definitions,
            &policy,
        );
        let row = rows.iter().find(|row| row.name == INSPECTION_TODO).unwrap();
        assert_eq!(row.state, expected);
        assert_eq!(
            row.note,
            Some(if disabled {
                REASON_CONFIG
            } else if expected == ToolState::Off {
                PROFILE_DISABLED
            } else {
                PROFILE_LOADING
            })
        );
        assert_eq!(
            catalog_row(&rows, &[]).is_some(),
            expected == ToolState::Lazy
        );
        let catalog = inspection_catalog(definitions, None);
        let tools = catalog.as_array().unwrap();
        assert_eq!(
            tools.iter().any(|tool| tool["name"] == INSPECTION_TODO),
            expected == ToolState::On
        );
        let search = tools
            .iter()
            .find(|tool| tool["name"] == TOOL_SEARCH_TOOL_NAME);
        assert_eq!(search.is_some(), expected == ToolState::Lazy);
        if let Some(search) = search {
            assert!(
                search["description"]
                    .as_str()
                    .unwrap()
                    .contains(INSPECTION_TODO)
            );
            assert!(
                !search["description"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("\n- {INSPECTION_TASK}:"))
            );
        }
    }

    #[test_case("eager"; "eager")]
    #[test_case("lazy"; "lazy")]
    fn tools_listing_explains_plan_mode_requirement(exposure: &str) {
        let registry = ToolRegistry::new();
        caudra_agent::tools::native::register(&registry, FeatureFlags::default()).unwrap();
        let config = RawConfig::default().into_config(false).unwrap();
        let model = Model::from_spec(BEST_SPEC).unwrap();
        let filter = ToolFilter::from_config(&config.agent, &model, &[]);
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "default": "disabled", "overrides": {PLAN_TOOL_NAME: exposure}
        }))
        .unwrap();
        let definitions = inspection_definitions(
            &registry,
            &Vars::new(),
            &DescriptionContext {
                filter: &filter,
                audience: ToolAudience::MAIN,
                workflows_available: false,
            },
            &config.agent,
            &model,
            &policy,
            &AgentMode::Build,
        )
        .unwrap();
        let rows = builtin_rows(
            &registry,
            &filter,
            &config,
            &[],
            &model,
            &definitions,
            &policy,
        );
        let plan = rows.iter().find(|row| row.name == PLAN_TOOL_NAME).unwrap();
        assert_eq!(plan.state, ToolState::Off);
        assert_eq!(plan.note, Some(PLAN_MODE_REQUIRED));
    }

    #[test_case(AgentMode::Build, ToolAudience::MAIN, false; "build")]
    #[test_case(AgentMode::Plan(PROMPT_PLAN_PATH.into()), ToolAudience::MAIN, true; "active_plan")]
    #[test_case(AgentMode::RemotePlan(PlanRef::new(PROMPT_PLAN_REFERENCE).unwrap()), ToolAudience::MAIN, true; "remote_plan")]
    #[test_case(AgentMode::Plan(PROMPT_PLAN_PATH.into()), ToolAudience::RESEARCH_SUB, false; "task_cannot_use_parent_plan")]
    fn inspection_plan_needs_an_active_target(
        mode: AgentMode,
        audience: ToolAudience,
        available: bool,
    ) {
        let registry = ToolRegistry::new();
        caudra_agent::tools::native::register(&registry, FeatureFlags::default()).unwrap();
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "default": "disabled", "overrides": {"plan": "lazy"}
        }))
        .unwrap();
        let definitions = inspection_definitions(
            &registry,
            &Vars::new(),
            &DescriptionContext {
                filter: &ToolFilter::All,
                audience,
                workflows_available: false,
            },
            &AgentConfig::default(),
            &Model::from_spec(BEST_SPEC).unwrap(),
            &policy,
            &mode,
        )
        .unwrap();
        assert_eq!(
            inspection_tool_state(&definitions, "plan"),
            if available {
                ToolState::Lazy
            } else {
                ToolState::Off
            }
        );
        let reminder = caudra_agent::prompt::plan_mode_prompt(&mode, |name| {
            inspection_tool_state(&definitions, name).reaches_model()
        });
        assert_eq!(reminder.is_some(), mode.is_planning());
        if let Some(reminder) = reminder {
            assert_eq!(reminder.contains("`plan`"), available);
            assert_eq!(
                reminder.contains(PROMPT_PLAN_REFERENCE),
                mode.plan_ref().is_some()
            );
            assert!(!reminder.contains("{plan_"));
            assert!(!reminder.contains("`shell`"));
            assert!(!reminder.contains("`file_write`"));
        }
        assert_eq!(
            inspection_catalog(definitions, None)
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == TOOL_SEARCH_TOOL_NAME),
            available
        );
    }

    #[test_case("eager"; "declared")]
    #[test_case("lazy"; "deferred")]
    fn inspection_shapes_execution_for_profile_tools(exposure: &str) {
        let registry = ToolRegistry::new();
        caudra_agent::tools::native::register(&registry, FeatureFlags::default()).unwrap();
        let config = AgentConfig {
            task_execution: ExecutionMode::Sync,
            ..AgentConfig::default()
        };
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "default": "disabled", "overrides": {INSPECTION_TASK: exposure}
        }))
        .unwrap();
        let definitions = inspection_definitions(
            &registry,
            &Vars::new(),
            &DescriptionContext {
                filter: &ToolFilter::All,
                audience: ToolAudience::MAIN,
                workflows_available: false,
            },
            &config,
            &Model::from_spec(BEST_SPEC).unwrap(),
            &policy,
            &AgentMode::Build,
        )
        .unwrap();
        let task = definitions
            .declared
            .as_array()
            .unwrap()
            .iter()
            .chain(definitions.deferred.iter().map(|tool| &tool.definition))
            .find(|tool| tool["name"] == INSPECTION_TASK)
            .unwrap();
        assert!(
            task["input_schema"]["properties"]
                .get("background")
                .is_none()
        );
    }

    #[test]
    fn inspection_rejects_unbound_profile_selectors() {
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "overrides": {INSPECTION_UNKNOWN: "eager"}
        }))
        .unwrap();
        let error = inspection_definitions(
            &ToolRegistry::new(),
            &Vars::new(),
            &DescriptionContext {
                filter: &ToolFilter::All,
                audience: ToolAudience::MAIN,
                workflows_available: false,
            },
            &AgentConfig::default(),
            &Model::from_spec(BEST_SPEC).unwrap(),
            &policy,
            &AgentMode::Build,
        )
        .err()
        .unwrap();
        let message = format!("{error:#}");
        assert!(message.contains(INSPECTION_UNKNOWN));
        assert!(message.contains(INSPECTION_BINDING_ERROR));
    }

    /// The catalog has no registry entry, so the row exists exactly when the
    /// request would carry one: whenever any source has something deferred.
    #[test_case(&[ToolState::On], &[], false ; "nothing_lazy")]
    #[test_case(&[ToolState::Off], &[], false ; "disabled_is_not_lazy")]
    #[test_case(&[ToolState::Lazy], &[], true ; "lazy_builtin")]
    #[test_case(&[], &[ToolState::Lazy], true ; "lazy_mcp")]
    fn the_catalog_row_tracks_whether_anything_is_lazy(
        builtin: &[ToolState],
        mcp: &[ToolState],
        expected: bool,
    ) {
        let row = catalog_row(&state_rows(builtin), &state_rows(mcp));

        assert_eq!(row.is_some(), expected);
        if let Some(row) = row {
            assert_eq!(row.name, TOOL_SEARCH_TOOL_NAME);
            assert_eq!(row.state, ToolState::On);
            assert!(row.permission.is_none(), "search has no permission gate");
        }
    }
}
