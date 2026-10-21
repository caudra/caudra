use std::env;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};

use caudra_agent::AgentMode;
use caudra_agent::decisions::{remove_decision_endpoint, save_decision_endpoint};
use caudra_agent::mcp::{McpSession, config as mcp_config, oauth as mcp_oauth};
use caudra_agent::prompt::profile::{PromptProfileCatalog, SystemPromptProfile};
use caudra_agent::template::{self, Vars};
use caudra_agent::tools::native::skill::{self, SkillDirCandidate, SkillInventoryEntry};
use caudra_agent::tools::profile_policy::{CEILING_DISABLED, LEGACY_LOADING, registered_decision};
use caudra_agent::tools::registry::ToolDefinitions;
use caudra_agent::tools::{
    DescriptionContext, ToolAudience, ToolFilter, ToolRegistry, ToolState, builtin_report,
    execution, is_tool_enabled,
};
use caudra_config::decisions::{
    DECISION_PRESETS, DecisionEndpointDef, DecisionPreset, DecisionProtocol,
};
use caudra_config::providers::{
    Protocol, ProviderDef, ProvidersConfig, ProvidersFile, all_builtins, builtin_provider,
    custom_provider_slug, resolve_api_key_env, resolve_base_url, resolve_default_model,
    resolve_display_name, resolve_login_url, slugify,
};
use caudra_config::{
    AgentConfig, Config, DefaultEffect, ModelPolicy, PermissionsConfig, ProfileToolPolicy, ToolKey,
};
use caudra_providers::copilot_auth::{CopilotAccount, CopilotIdentity, EnterpriseHost};
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::{fetch_all_models, seed_setup_thinking};
use caudra_providers::{
    Model, ModelMarker, ModelPurpose, ProviderData, ThinkingConfig, Timeouts, catalog_providers,
};
use caudra_providers::{anthropic_auth, copilot_auth, dynamic, openai_auth, xai_auth};
use caudra_storage::StateDir;
use caudra_storage::accounts::{AccountName, StoredSecret, list_accounts};
use caudra_storage::auth::{
    MAX_WORKCELL_BEARER_TOKEN_BYTES, McpAuthData, WorkcellCredential, WorkcellCredentialName,
    delete_workcell_credential, list_workcell_credentials, load_decision_credentials,
    load_mcp_auth, save_mcp_auth, save_workcell_credential, validate_name,
};
use caudra_storage::decisions::read_active_decision_endpoint;
use caudra_storage::model::persist_model_for_every_mode;
use caudra_storage::sessions::StoredMode;
use caudra_workspace::PlanRef;

use super::accounts;
use super::decisions::{self, DecisionKey, DecisionRow};
use crate::cli::{AuthMethod, Cli, normalize_tool_name};
use crate::setup::resolve_model;

const PROMPT_PLAN_PATH: &str = "<active plan document>";
const PROMPT_PLAN_REFERENCE: &str = "active-plan-inspection";
const REASON_INSPECTION_UNAVAILABLE: &str = "unavailable for this inspection runtime or mode";
const REASON_VIA_CODE: &str = "via code";
const AUTH_STATUS_EMPTY: &str = "       ";
const AUTH_STATUS_ENV: &str = "\x1b[33m~ env  \x1b[0m";
const AUTH_STATUS_KEY: &str = "\x1b[32m✓ key  \x1b[0m";
const AUTH_STATUS_OAUTH: &str = "\x1b[32m✓ oauth\x1b[0m";
const AUTH_STATUS_TOKEN: &str = "\x1b[32m✓ token\x1b[0m";
const METHOD_SCOPE_ERR: &str = "--method is supported only for Anthropic, OpenAI, xAI, and Copilot";
const COPILOT_METHOD_ERR: &str = "Copilot supports --method oauth or --method import";
const HOSTNAME_SCOPE_ERR: &str = "--hostname is supported only for copilot-enterprise";
const ENTERPRISE_HOST_PROMPT: &str = "  GitHub Enterprise hostname (e.g. company.ghe.com): ";
const ENTERPRISE_HOST_REQUIRED: &str =
    "copilot-enterprise needs a hostname; pass --hostname company.ghe.com";
pub(super) const PROVIDER_SLUG_WIDTH: usize = 14;
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
const CUSTOM_DECISION_TARGET: &str = "custom";
const DECISION_FLAGS_ERR: &str = "--method and --hostname do not apply to --decision";
const DECISION_MENU_HEADING: &str = "Decision models:";
const CUSTOM_DECISION_LABEL: &str = "Custom decision endpoint...";
const DECISION_STATUS_HEADING: &str = "\x1b[1mDecision endpoints:\x1b[0m";
const DECISION_ACTIVE_MARKER: &str = " \x1b[1m(active)\x1b[0m";
const DECISION_BADGE_SAVED: &str = "\x1b[32m✓\x1b[0m";
const DECISION_BADGE_ENV: &str = "\x1b[33m~\x1b[0m";
const DECISION_BADGE_CONFIGURED: &str = "\x1b[34m●\x1b[0m";
const PLAIN_HTTP_WARNING: &str = "Plain HTTP sends decision context and the key unencrypted; use it only over a tunnel you trust.";
const INPUT_CLOSED: &str = "input closed before an answer was entered";

#[derive(Debug, PartialEq, Eq)]
enum LoginRoute {
    AnthropicOauth,
    OpenAiOauth,
    XaiOauth,
    CopilotOauth(CopilotIdentity),
    CopilotImport(CopilotIdentity),
    ApiKey,
    Decision(DecisionTarget),
}

#[derive(Debug, PartialEq, Eq)]
enum DecisionTarget {
    Menu,
    Custom,
    /// A preset or a configured endpoint.
    Named(String),
}

impl DecisionTarget {
    fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase() {
            name if name == CUSTOM_DECISION_TARGET => Self::Custom,
            name => Self::Named(name),
        }
    }
}

pub fn auth_login(
    provider: Option<&str>,
    decision: bool,
    method: Option<AuthMethod>,
    hostname: Option<&str>,
    name: Option<AccountName>,
    storage: &StateDir,
) -> Result<()> {
    match provider {
        Some(provider) if decision => login_slug(provider, true, method, hostname, None, storage)?,
        Some(provider) => login_slug(&slugify(provider), false, method, hostname, name, storage)?,
        None if decision => login_decision(DecisionTarget::Menu, storage)?,
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

fn login_slug(
    slug: &str,
    decision: bool,
    method: Option<AuthMethod>,
    hostname: Option<&str>,
    name: Option<AccountName>,
    storage: &StateDir,
) -> Result<()> {
    let route = login_route(slug, decision, method, hostname.is_some())?;
    let login = match route {
        LoginRoute::Decision(target) => return login_decision(target, storage),
        LoginRoute::ApiKey => return login_api_key_slug(slug, name, storage),
        LoginRoute::AnthropicOauth => anthropic_auth::login()?,
        LoginRoute::OpenAiOauth => openai_auth::login()?,
        LoginRoute::XaiOauth => xai_auth::login()?,
        LoginRoute::CopilotOauth(identity) => {
            copilot_auth::login(&copilot_account(identity, hostname)?)?
        }
        LoginRoute::CopilotImport(identity) => {
            copilot_auth::import(&copilot_account(identity, hostname)?)?
        }
    };
    accounts::save(storage, slug, login, name)?;
    Ok(())
}

fn login_route(
    slug: &str,
    decision: bool,
    method: Option<AuthMethod>,
    has_hostname: bool,
) -> Result<LoginRoute> {
    if decision {
        if method.is_some() || has_hostname {
            bail!(DECISION_FLAGS_ERR);
        }
        return Ok(LoginRoute::Decision(DecisionTarget::parse(slug)));
    }
    let copilot = CopilotIdentity::from_slug(slug);
    if has_hostname && copilot != Some(CopilotIdentity::Enterprise) {
        bail!(HOSTNAME_SCOPE_ERR);
    }
    if let Some(identity) = copilot {
        return match method {
            None | Some(AuthMethod::Oauth) => Ok(LoginRoute::CopilotOauth(identity)),
            Some(AuthMethod::Import) => Ok(LoginRoute::CopilotImport(identity)),
            Some(AuthMethod::ApiKey) => Err(eyre!(COPILOT_METHOD_ERR)),
        };
    }
    match (slug, method) {
        ("anthropic" | "openai" | "xai", Some(AuthMethod::ApiKey)) => Ok(LoginRoute::ApiKey),
        ("anthropic", None | Some(AuthMethod::Oauth)) => Ok(LoginRoute::AnthropicOauth),
        ("openai", None | Some(AuthMethod::Oauth)) => Ok(LoginRoute::OpenAiOauth),
        ("xai", None | Some(AuthMethod::Oauth)) => Ok(LoginRoute::XaiOauth),
        (_, Some(_)) => Err(eyre!(METHOD_SCOPE_ERR)),
        _ => Ok(LoginRoute::ApiKey),
    }
}

fn copilot_account(identity: CopilotIdentity, hostname: Option<&str>) -> Result<CopilotAccount> {
    Ok(match (identity, hostname) {
        (CopilotIdentity::Public, _) => CopilotAccount::Public,
        (CopilotIdentity::Enterprise, Some(hostname)) => {
            CopilotAccount::Enterprise(EnterpriseHost::parse(hostname)?)
        }
        (CopilotIdentity::Enterprise, None) => {
            CopilotAccount::Enterprise(prompt_enterprise_host()?)
        }
    })
}

fn prompt_enterprise_host() -> Result<EnterpriseHost> {
    if !io::stdin().is_terminal() {
        bail!(ENTERPRISE_HOST_REQUIRED);
    }
    print!("{ENTERPRISE_HOST_PROMPT}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(EnterpriseHost::parse(&input)?)
}

fn env_credential(slug: &str, default_env: &'static str) -> Option<&'static str> {
    match CopilotIdentity::from_slug(slug) {
        Some(identity) => copilot_auth::env_override(identity),
        None => env::var(default_env).is_ok().then_some(default_env),
    }
}

fn login_api_key_slug(slug: &str, name: Option<AccountName>, storage: &StateDir) -> Result<()> {
    if builtin_provider(slug).is_none()
        && dynamic::display_name(slug).is_none()
        && !ProvidersConfig::load().defines_provider(slug)
        && let Some(provider_data) = caudra_providers::catalog_provider(slug)
    {
        login_catalog_provider(&provider_data, name, storage)
    } else {
        login_provider(slug, name, storage)
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
        _ => {
            bail!("invalid selection");
        }
    }
}

fn login_provider(slug: &str, name: Option<AccountName>, storage: &StateDir) -> Result<()> {
    let builtin = builtin_provider(slug);
    let is_custom = ProvidersConfig::load().defines_provider(slug);
    if builtin.is_none() && dynamic::display_name(slug).is_none() && !is_custom {
        bail!("unknown provider '{slug}'");
    }

    if builtin.is_none() && dynamic::auth_providers().iter().any(|(s, _)| *s == slug) {
        dynamic::login(slug)?;
        return Ok(());
    }

    let mut config = ProvidersFile::load()?;
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
        accounts::save_key(storage, slug, api_key, name)?;
    }

    if plan.is_some() || needs_url || host_url.is_some() || builtin.is_none() {
        config
            .upsert(slug.to_string(), provider_def)
            .context("update providers.toml")?;
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
    if !has_key {
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
        .filter(|s| builtin_provider(s).is_none() && *s != "opencode" && config.defines_provider(s))
        .collect();
    let catalog_entries = catalog_providers();
    let decision_entries = decision_entries(&config);
    let menu = LoginMenu {
        builtins: builtins.len(),
        custom_slugs: custom_slugs.len(),
        catalog: catalog_entries.len(),
        decisions: decision_entries.len(),
    };
    let custom_idx = menu.custom_idx();
    let last_idx = menu.last_idx();
    let number_width = last_idx.to_string().len();
    let slug_width = builtins
        .iter()
        .map(|provider| provider.slug.len())
        .chain(custom_slugs.iter().map(|slug| slug.len()))
        .chain(catalog_entries.iter().map(|provider| provider.slug.len()))
        .chain(decision_entries.iter().map(|entry| entry.name.len()))
        .max()
        .unwrap_or(PROVIDER_SLUG_WIDTH)
        .max(PROVIDER_SLUG_WIDTH);

    println!();
    println!("  Available providers:");
    println!();
    for (i, b) in builtins.iter().enumerate() {
        let status = stored_badge(storage, b.slug)
            .or_else(|| env_credential(b.slug, b.default_api_key_env).map(|_| AUTH_STATUS_ENV))
            .unwrap_or(AUTH_STATUS_EMPTY);
        let number = i + 1;
        let slug = b.slug;
        let display = b.display_name;
        println!("  {status} {number:>number_width$}. {slug:<slug_width$} {display}");
    }
    let mut idx = builtins.len();
    for slug in &custom_slugs {
        idx += 1;
        let status = stored_badge(storage, slug).unwrap_or(AUTH_STATUS_EMPTY);
        let display = config
            .get(slug)
            .and_then(|d| d.display_name.as_deref())
            .unwrap_or(slug);
        println!("  {status} {idx:>number_width$}. {slug:<slug_width$} {display}");
    }

    for cat in &catalog_entries {
        idx += 1;
        let status = stored_badge(storage, &cat.slug).unwrap_or(AUTH_STATUS_EMPTY);
        let slug = &cat.slug;
        let display = &cat.display_name;
        println!("  {status} {idx:>number_width$}. {slug:<slug_width$} {display}");
    }
    println!("  {AUTH_STATUS_EMPTY} {custom_idx:>number_width$}. Custom provider...");
    println!();
    print_decision_menu(
        &decision_entries,
        storage,
        custom_idx + 1,
        number_width,
        slug_width,
    )?;

    print!("  Select [1-{last_idx}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice: usize = input.trim().parse().context("enter a number")?;

    match menu
        .choice(choice)
        .ok_or_else(|| eyre!("invalid selection"))?
    {
        LoginChoice::CustomProvider => login_custom(storage)?,
        LoginChoice::Builtin(index) => {
            let slug = builtins[index].slug;
            let method = match slug {
                "anthropic" | "openai" => Some(prompt_auth_method(&resolve_display_name(
                    slug,
                    config.get(slug),
                ))?),
                _ => None,
            };
            login_slug(slug, false, method, None, None, storage)?;
        }
        LoginChoice::CustomSlug(index) => login_provider(custom_slugs[index], None, storage)?,
        LoginChoice::Catalog(index) => {
            login_catalog_provider(&catalog_entries[index], None, storage)?
        }
        LoginChoice::Decision(choice) => configure_decision(choice, decision_entries, storage)?,
    }

    Ok(())
}

/// How the login menu marks a provider with a stored account: OAuth wins,
/// since one subscription is what most users came to sign in with.
fn stored_badge(storage: &StateDir, slug: &str) -> Option<&'static str> {
    let accounts = list_accounts(storage, slug).ok()?;
    if accounts
        .iter()
        .any(|(_, account)| matches!(account.secret, StoredSecret::OAuth(_)))
    {
        return Some(AUTH_STATUS_OAUTH);
    }
    accounts.first()?;
    Some(match CopilotIdentity::from_slug(slug) {
        Some(_) => AUTH_STATUS_TOKEN,
        None => AUTH_STATUS_KEY,
    })
}

/// Numbering of the `auth login` menu: built-in, custom and catalog providers,
/// "Custom provider...", then the decision block.
struct LoginMenu {
    builtins: usize,
    custom_slugs: usize,
    catalog: usize,
    decisions: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum LoginChoice {
    Builtin(usize),
    CustomSlug(usize),
    Catalog(usize),
    CustomProvider,
    Decision(DecisionChoice),
}

#[derive(Debug, PartialEq, Eq)]
enum DecisionChoice {
    Endpoint(usize),
    Custom,
}

impl LoginMenu {
    fn custom_idx(&self) -> usize {
        self.builtins + self.custom_slugs + self.catalog + 1
    }

    fn last_idx(&self) -> usize {
        self.custom_idx() + self.decisions + 1
    }

    fn choice(&self, number: usize) -> Option<LoginChoice> {
        let custom_slugs_end = self.builtins + self.custom_slugs;
        let custom_idx = self.custom_idx();
        match number {
            0 => None,
            number if number <= self.builtins => Some(LoginChoice::Builtin(number - 1)),
            number if number <= custom_slugs_end => {
                Some(LoginChoice::CustomSlug(number - self.builtins - 1))
            }
            number if number < custom_idx => {
                Some(LoginChoice::Catalog(number - custom_slugs_end - 1))
            }
            number if number == custom_idx => Some(LoginChoice::CustomProvider),
            number => {
                decision_choice(self.decisions, number - custom_idx).map(LoginChoice::Decision)
            }
        }
    }
}

/// `number` counted from 1 within a decision block of `endpoints` rows and
/// the trailing custom row.
fn decision_choice(endpoints: usize, number: usize) -> Option<DecisionChoice> {
    match number {
        0 => None,
        number if number <= endpoints => Some(DecisionChoice::Endpoint(number - 1)),
        number if number == endpoints + 1 => Some(DecisionChoice::Custom),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DecisionEntry {
    name: String,
    label: String,
    def: DecisionEndpointDef,
}

/// The presets, each as configured when it is, then every other configured
/// endpoint.
fn decision_entries(providers: &ProvidersConfig) -> Vec<DecisionEntry> {
    let presets = DECISION_PRESETS.iter().map(|preset| DecisionEntry {
        name: preset.name.into(),
        label: preset.display_name.into(),
        def: providers
            .decisions
            .get(preset.name)
            .cloned()
            .unwrap_or_else(|| preset.def()),
    });
    let configured = providers
        .decisions
        .iter()
        .filter(|(name, _)| DecisionPreset::find(name).is_none())
        .map(|(name, def)| DecisionEntry {
            name: name.clone(),
            label: endpoint_summary(def),
            def: def.clone(),
        });
    presets.chain(configured).collect()
}

fn endpoint_summary(def: &DecisionEndpointDef) -> String {
    format!(
        "{} · {}",
        def.protocol.as_str(),
        def.model
            .as_deref()
            .unwrap_or_else(|| def.protocol.default_model())
    )
}

fn print_decision_menu(
    entries: &[DecisionEntry],
    storage: &StateDir,
    first: usize,
    number_width: usize,
    name_width: usize,
) -> Result<()> {
    let active = read_active_decision_endpoint(storage);
    println!("  {DECISION_MENU_HEADING}");
    println!();
    for (number, entry) in (first..).zip(entries) {
        let status = match decisions::decision_key(
            storage,
            &entry.name,
            entry.def.api_key_env.as_deref(),
        )? {
            DecisionKey::Saved(_) => AUTH_STATUS_KEY,
            DecisionKey::Env(_) => AUTH_STATUS_ENV,
            DecisionKey::Missing => AUTH_STATUS_EMPTY,
        };
        let marker = if active.as_deref() == Some(entry.name.as_str()) {
            DECISION_ACTIVE_MARKER
        } else {
            ""
        };
        let DecisionEntry { name, label, .. } = entry;
        println!("  {status} {number:>number_width$}. {name:<name_width$} {label}{marker}");
    }
    let custom = first + entries.len();
    println!("  {AUTH_STATUS_EMPTY} {custom:>number_width$}. {CUSTOM_DECISION_LABEL}");
    println!();
    Ok(())
}

fn login_decision(target: DecisionTarget, storage: &StateDir) -> Result<()> {
    let entries = decision_entries(&ProvidersConfig::load());
    let choice = match target {
        DecisionTarget::Menu => {
            let last = entries.len() + 1;
            let width = last.to_string().len();
            let name_width = entries
                .iter()
                .map(|entry| entry.name.len())
                .max()
                .unwrap_or_default()
                .max(PROVIDER_SLUG_WIDTH);
            println!();
            print_decision_menu(&entries, storage, 1, width, name_width)?;
            let input = prompt_line(&format!("  Select [1-{last}]: "))?;
            let number = input.parse().context("enter a number")?;
            decision_choice(entries.len(), number).ok_or_else(|| eyre!("invalid selection"))?
        }
        DecisionTarget::Custom => DecisionChoice::Custom,
        DecisionTarget::Named(name) => entries
            .iter()
            .position(|entry| entry.name == name)
            .map(DecisionChoice::Endpoint)
            .ok_or_else(|| {
                let presets: Vec<&str> = DECISION_PRESETS.iter().map(|preset| preset.name).collect();
                eyre!(
                    "unknown decision endpoint '{name}'; use a preset ({}), {CUSTOM_DECISION_TARGET}, or a configured endpoint name",
                    presets.join(", ")
                )
            })?,
    };
    configure_decision(choice, entries, storage)
}

fn configure_decision(
    choice: DecisionChoice,
    entries: Vec<DecisionEntry>,
    storage: &StateDir,
) -> Result<()> {
    let (name, def) = match choice {
        DecisionChoice::Endpoint(index) => {
            let entry = entries
                .into_iter()
                .nth(index)
                .ok_or_else(|| eyre!("invalid selection"))?;
            (entry.name, entry.def)
        }
        DecisionChoice::Custom => prompt_custom_decision()?,
    };
    let api_key = prompt_decision_key(storage, &name, &def)?;
    let saved = save_decision_endpoint(storage, &name, def, Some(&api_key))
        .with_context(|| format!("save decision endpoint '{name}'"))?;
    let endpoint = &saved.endpoint;
    let key = decisions::decision_key(storage, &name, endpoint.api_key_env.as_deref())?;
    println!();
    println!(
        "  \x1b[32m✓\x1b[0m Decision endpoint: {} ({} · {})",
        endpoint.name,
        endpoint.protocol.as_str(),
        endpoint.model
    );
    println!("  Base URL: {}", endpoint.base_url);
    match key {
        DecisionKey::Saved(masked) => println!("  Key: saved ({masked})"),
        DecisionKey::Env(variable) => println!("  Key: via {variable}"),
        DecisionKey::Missing => println!("  Key: none"),
    }
    if endpoint.allow_http {
        println!("  \x1b[33m!\x1b[0m {PLAIN_HTTP_WARNING}");
    }
    if saved.active {
        println!("  Active: new sessions screen with it");
    } else {
        println!(
            "  Not active; switch with: caudra decisions use {}",
            endpoint.name
        );
    }
    Ok(())
}

fn prompt_decision_key(
    storage: &StateDir,
    name: &str,
    def: &DecisionEndpointDef,
) -> Result<String> {
    if let Some(preset) = DecisionPreset::find(name) {
        println!("  Get a key at {}", preset.key_url);
    }
    let stored = load_decision_credentials(storage, name)
        .with_context(|| format!("load the key for decision endpoint '{name}'"))?
        .is_some();
    let hint = match (&def.api_key_env, stored) {
        (_, true) => "Enter keeps the saved key".to_owned(),
        (Some(variable), false) => format!("Enter falls back to {variable}"),
        (None, false) => "Enter to skip".to_owned(),
    };
    prompt_line(&format!("  {name} API key ({hint}): "))
}

fn decision_protocol(input: &str) -> Option<DecisionProtocol> {
    match input.trim() {
        "1" => Some(DecisionProtocol::TypeSafe),
        "2" => Some(DecisionProtocol::OpenAI),
        input => DecisionProtocol::ALL
            .into_iter()
            .find(|protocol| protocol.as_str() == input),
    }
}

fn confirmed(input: &str) -> bool {
    matches!(input.to_ascii_lowercase().as_str(), "y" | "yes")
}

fn prompt_custom_decision() -> Result<(String, DecisionEndpointDef)> {
    let name = loop {
        let name = prompt_line("  Endpoint name: ")?;
        match validate_name(&name) {
            Ok(()) => break name,
            Err(error) => println!("  {error}"),
        }
    };

    println!("  Protocol:");
    for (number, protocol) in (1..).zip(DecisionProtocol::ALL) {
        println!(
            "    {number}. {:<9} (appends {})",
            protocol.as_str(),
            protocol.endpoint_path()
        );
    }
    let protocol = loop {
        match decision_protocol(&prompt_line("  Select [1-2]: ")?) {
            Some(protocol) => break protocol,
            None => println!("  invalid protocol selection"),
        }
    };

    let mut def = DecisionEndpointDef {
        base_url: String::new(),
        model: None,
        api_key_env: None,
        allow_http: None,
        protocol,
    };
    println!(
        "  The base URL is the API base, not the full endpoint: Caudra appends {}.",
        def.protocol.endpoint_path()
    );
    loop {
        def.base_url = prompt_line(&format!(
            "  Base URL (e.g. {}): ",
            def.protocol.preset().base_url
        ))?;
        def.allow_http = None;
        if def.needs_allow_http() {
            println!("  \x1b[33m!\x1b[0m {PLAIN_HTTP_WARNING}");
            if !confirmed(&prompt_line(
                "  Send decision context over plain HTTP? [y/N]: ",
            )?) {
                continue;
            }
            def.allow_http = Some(true);
        }
        match def.resolve(&name) {
            Ok(_) => break,
            Err(error) => println!("  {error}"),
        }
    }

    loop {
        let model = prompt_line(&format!("  Model [{}]: ", def.protocol.default_model()))?;
        def.model = (!model.is_empty()).then_some(model);
        match def.resolve(&name) {
            Ok(_) => break,
            Err(error) => println!("  {error}"),
        }
    }
    Ok((name, def))
}

/// One trimmed line from stdin. Closed input is an error, so a re-prompting
/// loop never spins on a pipe that has ended.
fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        bail!(INPUT_CLOSED);
    }
    Ok(input.trim().to_owned())
}

fn login_catalog_provider(
    provider: &ProviderData,
    name: Option<AccountName>,
    storage: &StateDir,
) -> Result<()> {
    println!();
    if let Some(ref var) = provider.env_keys.first() {
        println!("  Provider: {} (env: {var})", provider.slug);
    } else {
        println!("  Provider: {}", provider.slug);
    }
    let key = accounts::read_key("  API key: ")?;
    if key.is_empty() {
        println!("  Skipped (no key entered)");
        return Ok(());
    }
    accounts::save_key(storage, &provider.slug, key, name)?;
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

    let api_key = accounts::read_key("  API key (or Enter to skip): ")?;

    let mut config = ProvidersFile::load()?;
    let provider_def = ProviderDef {
        display_name: Some(display_name),
        protocol: Some(protocol),
        base_url: Some(base_url.clone()),
        api_key_env: Some(api_key_env.clone()),
        discover_models: true,
        ..Default::default()
    };

    config
        .upsert(slug.clone(), provider_def)
        .context("update providers.toml")?;
    config.save().context("save providers.toml")?;

    let has_key = !api_key.is_empty();
    if has_key {
        accounts::save_key(storage, &slug, api_key, None)?;
    }

    println!();
    println!("  \x1b[32m✓\x1b[0m Configured: {}", slug);
    println!("  Endpoint: {}", base_url);
    if !has_key {
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
    let prompt = if optional {
        format!("  {display_name} API key (or Enter to skip): ")
    } else {
        format!("  {display_name} API key: ")
    };
    accounts::read_key(&prompt)
}

/// Signs out of provider accounts and leaves `providers.toml` alone, so a
/// configured provider and the accounts it lists stay for the next login. A
/// scripted provider with its own sign-in signs out through its script.
pub fn auth_logout(
    provider: &str,
    name: Option<AccountName>,
    all: bool,
    storage: &StateDir,
) -> Result<()> {
    let slug = slugify(provider);
    let scripted = builtin_provider(&slug).is_none()
        && dynamic::auth_providers().iter().any(|(s, _)| *s == slug);
    if scripted && list_accounts(storage, &slug)?.is_empty() {
        dynamic::logout(&slug)?;
        return Ok(());
    }
    accounts::logout(storage, &slug, name, all)
}

pub fn decision_logout(name: &str, storage: &StateDir) -> Result<()> {
    let name = name.trim().to_ascii_lowercase();
    let removed = remove_decision_endpoint(storage, &name)
        .with_context(|| format!("remove decision endpoint '{name}'"))?;
    if !removed.removed {
        println!("Decision endpoint '{name}' was not configured.");
        return Ok(());
    }
    println!("Removed decision endpoint '{name}'.");
    if removed.deactivated {
        println!("It was the active endpoint; decisions are off.");
    }
    Ok(())
}

fn render_decision_status(rows: &[DecisionRow]) -> String {
    let mut output = format!("  {DECISION_STATUS_HEADING}\n");
    if rows.is_empty() {
        let _ = writeln!(
            output,
            "  none configured, {}",
            decisions::DECISION_LOGIN_HINT
        );
    }
    for row in rows {
        let (badge, detail) = match &row.key {
            DecisionKey::Saved(masked) => (DECISION_BADGE_SAVED, format!("key: {masked}")),
            DecisionKey::Env(variable) => (DECISION_BADGE_ENV, format!("via {variable}")),
            DecisionKey::Missing => (DECISION_BADGE_CONFIGURED, "configured".to_owned()),
        };
        let marker = if row.active {
            DECISION_ACTIVE_MARKER
        } else {
            ""
        };
        let _ = writeln!(
            output,
            "  {badge} {:<PROVIDER_SLUG_WIDTH$} {} · {} ({detail}){marker}",
            row.name, row.protocol, row.model
        );
    }
    output.push('\n');
    output
}

pub fn auth_status(storage: &StateDir, refresh: bool) -> Result<()> {
    let config = ProvidersConfig::load();

    println!();
    for b in &all_builtins() {
        let def = config.get(b.slug);
        let configured = def.is_some_and(|d| d.base_url.is_some());
        print_provider_status(
            b.slug,
            &resolve_display_name(b.slug, def),
            refresh,
            configured,
        );
    }
    for (slug, def) in &config.providers {
        // 'opencode' could show up here, when the user configured free models on that provider.
        if builtin_provider(slug).is_some()
            || !config.defines_provider(slug)
            || (slug == "opencode" && def.enable_free_models.is_some())
        {
            continue;
        }
        print_provider_status(
            slug,
            def.display_name.as_deref().unwrap_or(slug),
            refresh,
            false,
        );
    }
    print!(
        "{}",
        render_decision_status(&decisions::decision_rows(storage, &config)?)
    );
    let catalog_entries = catalog_providers();
    if !catalog_entries.is_empty() {
        println!("  \x1b[1mCatalog Providers (models.dev):\x1b[0m");
        for entry in &catalog_entries {
            print_provider_status(&entry.slug, &entry.display_name, refresh, false);
        }
        println!();
    }

    Ok(())
}

/// A provider that cannot list its accounts still gets its line, with the
/// reason, so one broken entry leaves the rest of the status readable.
fn print_provider_status(slug: &str, display: &str, refresh: bool, configured: bool) {
    match accounts::provider_status(slug, display, refresh) {
        Ok(Some(lines)) => print!("{lines}"),
        Ok(None) if configured => {
            println!("  \x1b[34m●\x1b[0m {slug:<PROVIDER_SLUG_WIDTH$} {display} (configured)");
        }
        Ok(None) => println!(
            "  \x1b[31m✗\x1b[0m {slug:<PROVIDER_SLUG_WIDTH$} {display} (run: caudra auth login {slug})"
        ),
        Err(error) => {
            println!("  \x1b[31m✗\x1b[0m {slug:<PROVIDER_SLUG_WIDTH$} {display} ({error})");
        }
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
                format!("{spec:<width$}{MODEL_COLUMN_GAP}{}", marker.label())
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
            _ => {
                bail!("server '{server}' is not an HTTP transport");
            }
        };
        let resolved_addresses = mcp_config::resolve_url_addresses(&url)
            .map_err(|error| color_eyre::eyre::eyre!("cannot resolve MCP URL: {error}"))?;
        let authenticate = || {
            mcp_oauth::authenticate(
                server,
                &url,
                None,
                storage,
                mcp_oauth::Interaction::Cli,
                oauth.clone(),
                Some(&resolved_addresses),
            )
        };
        let refused = async |data: &McpAuthData| match &data.tokens {
            Some(tokens) => {
                mcp_oauth::token_refused(&url, &tokens.access, Some(&resolved_addresses)).await
            }
            None => false,
        };
        let held_tokens =
            load_mcp_auth(storage, server, &url).is_some_and(|data| data.tokens.is_some());
        let data = authenticate().await?;
        if refused(&data).await {
            if !held_tokens {
                bail!("MCP server '{server}': {}", mcp_oauth::REFUSED_ACCESS);
            }
            eprintln!("MCP server '{server}' refused the stored sign-in; signing in again");
            save_mcp_auth(
                storage,
                server,
                &McpAuthData {
                    tokens: None,
                    ..data
                },
            )?;
            if refused(&authenticate().await?).await {
                bail!("MCP server '{server}': {}", mcp_oauth::REFUSED_ACCESS);
            }
        }
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
            let mut report = builtin_report(name, filter, cli_disallowed, &config.agent, model);
            let decision = registered_decision(entry, &ctx, policy, false);
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
                permission: permission_default(&config.permissions, &[ToolKey::native(name)]),
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
            let (state, note) = if tool.disabled {
                (
                    ToolState::Off,
                    tool.reason.unwrap_or(REASON_INSPECTION_UNAVAILABLE),
                )
            } else {
                (ToolState::On, REASON_VIA_CODE)
            };
            ToolRow {
                state,
                note: Some(note),
                permission: permission_default(permissions, &keys),
                source: format!("{SOURCE_MCP}:{}", tool.server),
                name: tool.qualified_name,
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
    session_plan: bool,
) -> Result<ToolDefinitions> {
    policy
        .validate_bindings(registry.iter().iter().map(|entry| entry.name()))
        .map_err(|error| eyre!(error))
        .context("resolve system prompt profile tools")?;
    let mut definitions = registry.definitions_split_with_policy(
        vars,
        ctx,
        model.supports_tool_examples(),
        policy,
        session_plan,
    );
    execution::configure_declared(
        &mut definitions.declared,
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
    let declared = definitions
        .declared
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"].as_str() == Some(name)));
    if declared || definitions.hosted.iter().any(|tool| tool == name) {
        ToolState::On
    } else {
        ToolState::Off
    }
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
        McpSession::new(handle)
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
    let profiles = PromptProfileCatalog::discover_user(config.agent.builtin_skills.pstack);
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
    let definitions =
        inspection_definitions(reg, &vars, &ctx, &config.agent, &model, &policy, false)?;
    if schemas {
        println!("{}", serde_json::to_string_pretty(&definitions.declared)?);
        return Ok(());
    }
    let mcp = inspection_mcp(
        &cwd,
        runtime.is_remote(),
        &config.agent,
        Arc::clone(&policy),
        &AgentMode::Build,
    );

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
    let mut mcp_tools = mcp_rows(mcp.as_ref(), &config.permissions);
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
            Err(message) => {
                bail!(message);
            }
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
    use caudra_agent::memory::baseline::{MemoryBaseline, open_store as open_memory_store};
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
    let prompt_profiles = PromptProfileCatalog::discover_user(config.agent.builtin_skills.pstack);
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
    let definitions = inspection_definitions(
        reg,
        &vars,
        &ctx,
        &config.agent,
        &model,
        &policy,
        mode.has_session_plan(None),
    )?;
    if tools {
        let defs = &definitions.declared;
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
            println!("{}", serde_json::to_string_pretty(defs)?);
        }
        return Ok(());
    }

    let slots = execution::execution_slots(
        &slots,
        &config.agent,
        audience == ToolAudience::MAIN,
        true,
        &definitions.declared,
    );
    let filter = definitions.available_filter().for_mode(&mode);
    let output = match variant {
        PromptVariant::System => {
            let memory = MemoryBaseline::adopt(
                open_memory_store(runtime.local_documents(), Some(&storage), &cwd),
                None,
            );
            let system = build_system_prompt(
                &instructions,
                &slots,
                &filter,
                system_prompt_profile.as_deref(),
                memory.view(),
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
    use caudra_agent::tools::native::plan::SESSION_PLAN_LABEL;
    use caudra_agent::tools::profile_policy::{
        PLAN_REQUIRED, PLAN_TOOL_NAME, PROFILE_DISABLED, PROFILE_LOADING,
    };
    use caudra_agent::tools::report::{REASON_CODE_MODULE, REASON_CONFIG};
    use caudra_agent::tools::{AUTOMATION_TOOL_NAME, cli_tool_ctx};
    use caudra_config::{Effect, ExecutionMode, Feature, FeatureFlags, PermissionRule, RawConfig};
    use caudra_workcell::WorkcellHost;
    use tempfile::TempDir;
    use test_case::test_case;

    const OPENAI: &str = "openai";
    const TYPESAFE: &str = "typesafe";
    const TUNNEL: &str = "tunnel";
    const TUNNEL_BASE_URL: &str = "http://100.64.0.2:8080/v1";
    const TUNNEL_MODEL: &str = "systemone-decision-v7";
    const TUNNEL_LABEL: &str = "openai · systemone-decision-v7";
    const MASKED_KEY: &str = "sk-1...cdef";
    const KEY_VARIABLE: &str = "TYPESAFE_API_KEY";
    const MENU: LoginMenu = LoginMenu {
        builtins: 2,
        custom_slugs: 1,
        catalog: 2,
        decisions: 3,
    };

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
    #[test_case("xai", Some(AuthMethod::Oauth), LoginRoute::XaiOauth ; "xai_oauth")]
    #[test_case("xai", Some(AuthMethod::ApiKey), LoginRoute::ApiKey ; "xai_api_key")]
    #[test_case("Copilot", None, LoginRoute::CopilotOauth(CopilotIdentity::Public) ; "normalized_copilot_defaults_to_oauth")]
    #[test_case("copilot", Some(AuthMethod::Import), LoginRoute::CopilotImport(CopilotIdentity::Public) ; "copilot_import")]
    #[test_case("Copilot Enterprise", None, LoginRoute::CopilotOauth(CopilotIdentity::Enterprise) ; "normalized_enterprise_defaults_to_oauth")]
    #[test_case("copilot-enterprise", Some(AuthMethod::Oauth), LoginRoute::CopilotOauth(CopilotIdentity::Enterprise) ; "enterprise_oauth")]
    #[test_case("copilot-enterprise", Some(AuthMethod::Import), LoginRoute::CopilotImport(CopilotIdentity::Enterprise) ; "enterprise_import")]
    #[test_case("google", None, LoginRoute::ApiKey ; "ordinary_provider")]
    fn provider_login_routes(raw: &str, method: Option<AuthMethod>, expected: LoginRoute) {
        assert_eq!(
            login_route(&slugify(raw), false, method, false).unwrap(),
            expected
        );
    }

    #[test_case("openai", DecisionTarget::Named(OPENAI.into()) ; "preset")]
    #[test_case(" TypeSafe ", DecisionTarget::Named(TYPESAFE.into()) ; "normalized_preset")]
    #[test_case("my_tunnel", DecisionTarget::Named("my_tunnel".into()) ; "underscores_survive")]
    #[test_case("custom", DecisionTarget::Custom ; "custom")]
    #[test_case("Custom", DecisionTarget::Custom ; "normalized_custom")]
    fn decision_login_routes(raw: &str, expected: DecisionTarget) {
        assert_eq!(
            login_route(raw, true, None, false).unwrap(),
            LoginRoute::Decision(expected)
        );
    }

    #[test_case(Some(AuthMethod::ApiKey), false ; "method")]
    #[test_case(None, true ; "hostname")]
    fn decision_login_rejects_provider_options(method: Option<AuthMethod>, has_hostname: bool) {
        let error = login_route(OPENAI, true, method, has_hostname).unwrap_err();
        assert_eq!(error.to_string(), DECISION_FLAGS_ERR);
    }

    #[test_case(MENU, 0, None ; "zero")]
    #[test_case(MENU, 1, Some(LoginChoice::Builtin(0)) ; "first_builtin")]
    #[test_case(MENU, 2, Some(LoginChoice::Builtin(1)) ; "last_builtin")]
    #[test_case(MENU, 3, Some(LoginChoice::CustomSlug(0)) ; "custom_slug")]
    #[test_case(MENU, 4, Some(LoginChoice::Catalog(0)) ; "first_catalog")]
    #[test_case(MENU, 5, Some(LoginChoice::Catalog(1)) ; "last_catalog")]
    #[test_case(MENU, 6, Some(LoginChoice::CustomProvider) ; "custom_provider")]
    #[test_case(MENU, 7, Some(LoginChoice::Decision(DecisionChoice::Endpoint(0))) ; "first_decision")]
    #[test_case(MENU, 9, Some(LoginChoice::Decision(DecisionChoice::Endpoint(2))) ; "last_decision")]
    #[test_case(MENU, 10, Some(LoginChoice::Decision(DecisionChoice::Custom)) ; "custom_decision")]
    #[test_case(MENU, 11, None ; "past_the_end")]
    #[test_case(LoginMenu { builtins: 1, custom_slugs: 0, catalog: 0, decisions: 2 }, 2, Some(LoginChoice::CustomProvider) ; "empty_sections_custom_provider")]
    #[test_case(LoginMenu { builtins: 1, custom_slugs: 0, catalog: 0, decisions: 2 }, 3, Some(LoginChoice::Decision(DecisionChoice::Endpoint(0))) ; "empty_sections_decision")]
    #[test_case(LoginMenu { builtins: 1, custom_slugs: 0, catalog: 0, decisions: 2 }, 5, Some(LoginChoice::Decision(DecisionChoice::Custom)) ; "empty_sections_custom_decision")]
    fn login_menu_numbers_every_section(
        menu: LoginMenu,
        number: usize,
        expected: Option<LoginChoice>,
    ) {
        assert_eq!(menu.choice(number), expected);
        assert!(menu.choice(menu.last_idx()).is_some());
        assert!(menu.choice(menu.last_idx() + 1).is_none());
    }

    #[test_case(2, 1, Some(DecisionChoice::Endpoint(0)) ; "first")]
    #[test_case(2, 3, Some(DecisionChoice::Custom) ; "custom")]
    #[test_case(0, 1, Some(DecisionChoice::Custom) ; "custom_only")]
    #[test_case(2, 0, None ; "zero")]
    #[test_case(2, 4, None ; "past_the_end")]
    fn decision_menu_numbers_endpoints_then_custom(
        endpoints: usize,
        number: usize,
        expected: Option<DecisionChoice>,
    ) {
        assert_eq!(decision_choice(endpoints, number), expected);
    }

    #[test]
    fn decision_entries_list_presets_then_configured_endpoints() {
        let mut providers = ProvidersConfig::default();
        let mut typesafe = DECISION_PRESETS[0].def();
        typesafe.model = Some(TUNNEL_MODEL.into());
        providers
            .decisions
            .insert(TYPESAFE.into(), typesafe.clone());
        providers.decisions.insert(
            TUNNEL.into(),
            DecisionEndpointDef {
                protocol: DecisionProtocol::OpenAI,
                base_url: TUNNEL_BASE_URL.into(),
                model: Some(TUNNEL_MODEL.into()),
                api_key_env: None,
                allow_http: Some(true),
            },
        );

        let entries = decision_entries(&providers);

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, [TYPESAFE, OPENAI, TUNNEL]);
        assert_eq!(entries[0].def, typesafe);
        assert_eq!(entries[0].label, DECISION_PRESETS[0].display_name);
        assert_eq!(entries[1].def, DECISION_PRESETS[1].def());
        assert_eq!(entries[2].label, TUNNEL_LABEL);
    }

    #[test_case("1", Some(DecisionProtocol::TypeSafe) ; "first")]
    #[test_case(" 2 ", Some(DecisionProtocol::OpenAI) ; "second")]
    #[test_case("openai", Some(DecisionProtocol::OpenAI) ; "by_name")]
    #[test_case("3", None ; "out_of_range")]
    fn decision_protocol_selection(input: &str, expected: Option<DecisionProtocol>) {
        assert_eq!(decision_protocol(input), expected);
    }

    #[test]
    fn decision_status_marks_key_sources_and_the_active_endpoint() {
        let row = |name: &str, key: DecisionKey, active: bool| DecisionRow {
            name: name.into(),
            protocol: DecisionProtocol::OpenAI.as_str(),
            model: TUNNEL_MODEL.into(),
            host: String::new(),
            key,
            active,
        };
        let rows = [
            row(OPENAI, DecisionKey::Saved(MASKED_KEY.into()), true),
            row(TYPESAFE, DecisionKey::Env(KEY_VARIABLE.into()), false),
            row(TUNNEL, DecisionKey::Missing, false),
        ];

        let rendered = render_decision_status(&rows);

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], format!("  {DECISION_STATUS_HEADING}"));
        assert_eq!(
            lines[1],
            format!(
                "  {DECISION_BADGE_SAVED} {OPENAI:<PROVIDER_SLUG_WIDTH$} {TUNNEL_LABEL} (key: {MASKED_KEY}){DECISION_ACTIVE_MARKER}"
            )
        );
        assert_eq!(
            lines[2],
            format!(
                "  {DECISION_BADGE_ENV} {TYPESAFE:<PROVIDER_SLUG_WIDTH$} {TUNNEL_LABEL} (via {KEY_VARIABLE})"
            )
        );
        assert_eq!(
            lines[3],
            format!(
                "  {DECISION_BADGE_CONFIGURED} {TUNNEL:<PROVIDER_SLUG_WIDTH$} {TUNNEL_LABEL} (configured)"
            )
        );
    }

    #[test]
    fn decision_status_without_endpoints_points_at_login() {
        let rendered = render_decision_status(&[]);
        assert!(rendered.contains(decisions::DECISION_LOGIN_HINT));
    }

    #[test_case("google", Some(AuthMethod::Oauth), false, METHOD_SCOPE_ERR ; "oauth_on_non_subscription_provider")]
    #[test_case("anthropic", Some(AuthMethod::Import), false, METHOD_SCOPE_ERR ; "import_on_anthropic")]
    #[test_case("copilot", Some(AuthMethod::ApiKey), false, COPILOT_METHOD_ERR ; "api_key_on_copilot")]
    #[test_case("copilot", None, true, HOSTNAME_SCOPE_ERR ; "hostname_on_public_copilot")]
    #[test_case("openai", None, true, HOSTNAME_SCOPE_ERR ; "hostname_on_openai")]
    fn invalid_login_options_are_rejected(
        slug: &str,
        method: Option<AuthMethod>,
        has_hostname: bool,
        expected: &str,
    ) {
        let error = login_route(slug, false, method, has_hostname).unwrap_err();
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn enterprise_hostname_is_validated() {
        let account = copilot_account(
            CopilotIdentity::Enterprise,
            Some("https://Company.ghe.com/"),
        )
        .unwrap();
        assert_eq!(account.host(), "company.ghe.com");
        assert!(
            copilot_account(CopilotIdentity::Enterprise, Some("http://company.ghe.com")).is_err()
        );
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

    const FAST_SPEC: &str = "anthropic/claude-haiku-5-5";
    const SMALL_SPEC: &str = "openai/gpt-5.4-nano";
    const BEST_SPEC: &str = "openai/gpt-5.6-sol";
    const UNCURATED_SPEC: &str = "ollama/llama3";
    const AGGREGATED_SPEC: &str = "openrouter/anthropic/claude-haiku-5-5";
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
            vec![format!("{spec}{MODEL_COLUMN_GAP}{}", marker.label())]
        );
    }

    /// An aggregator has no catalogue of its own, so the marker has to come
    /// from the vendor named in the model id.
    #[test]
    fn an_aggregated_model_borrows_the_upstream_marker() {
        let lines = model_lines(&[AGGREGATED_SPEC.into()]);
        assert!(lines[0].ends_with(ModelMarker::Fast.label()), "{lines:?}");
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

    #[test_case("eager", false, ToolState::On; "eager")]
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
            false,
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
            definitions
                .declared
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == INSPECTION_TODO),
            expected == ToolState::On
        );
    }

    #[test_case("eager"; "eager")]
    #[test_case("disabled"; "disabled")]
    fn tools_listing_explains_the_session_plan_requirement(exposure: &str) {
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
            false,
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
        assert_eq!(plan.note, Some(PLAN_REQUIRED));
    }

    #[test_case(AgentMode::Build, ToolAudience::MAIN, false; "build")]
    #[test_case(AgentMode::Plan(PROMPT_PLAN_PATH.into()), ToolAudience::MAIN, true; "active_plan")]
    #[test_case(AgentMode::RemotePlan(PlanRef::new(PROMPT_PLAN_REFERENCE).unwrap()), ToolAudience::MAIN, true; "remote_plan")]
    #[test_case(AgentMode::ReadOnly, ToolAudience::RESEARCH_SUB, false; "task_without_session_plan")]
    fn inspection_plan_needs_an_active_target(
        mode: AgentMode,
        audience: ToolAudience,
        available: bool,
    ) {
        let registry = ToolRegistry::new();
        caudra_agent::tools::native::register(&registry, FeatureFlags::default()).unwrap();
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "default": "disabled", "overrides": {"plan": "eager"}
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
            mode.has_session_plan(None),
        )
        .unwrap();
        assert_eq!(
            inspection_tool_state(&definitions, "plan"),
            if available {
                ToolState::On
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
                reminder.contains(PROMPT_PLAN_PATH),
                mode.plan_ref().is_none()
            );
            assert_eq!(
                reminder.contains(SESSION_PLAN_LABEL),
                mode.plan_ref().is_some()
            );
            assert!(!reminder.contains(PROMPT_PLAN_REFERENCE));
            assert!(!reminder.contains("{plan_"));
            assert!(!reminder.contains("`shell`"));
            assert!(!reminder.contains("`file_write`"));
        }
    }

    #[test]
    fn inspection_shapes_execution_for_profile_tools() {
        let registry = ToolRegistry::new();
        caudra_agent::tools::native::register(&registry, FeatureFlags::default()).unwrap();
        let config = AgentConfig {
            task_execution: ExecutionMode::Sync,
            ..AgentConfig::default()
        };
        let policy: ProfileToolPolicy = serde_json::from_value(serde_json::json!({
            "default": "disabled", "overrides": {INSPECTION_TASK: "eager"}
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
            false,
        )
        .unwrap();
        let task = definitions
            .declared
            .as_array()
            .unwrap()
            .iter()
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
            false,
        )
        .err()
        .unwrap();
        let message = format!("{error:#}");
        assert!(message.contains(INSPECTION_UNKNOWN));
        assert!(message.contains(INSPECTION_BINDING_ERROR));
    }

    #[test]
    fn a_code_hosted_tool_is_on_without_a_declaration() {
        let registry = ToolRegistry::new();
        caudra_agent::tools::native::register(
            &registry,
            FeatureFlags::NONE.with(Feature::Automations),
        )
        .unwrap();
        let mut config = RawConfig::default().into_config(false).unwrap();
        config.agent.features = FeatureFlags::NONE.with(Feature::Automations);
        let model = Model::from_spec(BEST_SPEC).unwrap();
        let filter = ToolFilter::from_config(&config.agent, &model, &[]);
        let policy = ProfileToolPolicy::default();
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
            false,
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
        let row = rows
            .iter()
            .find(|row| row.name == AUTOMATION_TOOL_NAME)
            .unwrap();

        assert_eq!(
            (row.state, row.note),
            (ToolState::On, Some(REASON_CODE_MODULE))
        );
        assert!(
            !definitions
                .declared
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == AUTOMATION_TOOL_NAME)
        );
    }
}
