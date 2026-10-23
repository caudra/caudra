//! The process-wide account pools, one per provider slug. A pool is the union
//! of the accounts `providers.toml` lists, the credentials `caudra auth login`
//! stored, the provider-level `api_key`, and the provider's environment
//! variable, in that priority order. Every routed provider of a slug shares
//! its pool's quota readings, conversation lineages, and per-account provider
//! instances, so `provider_for_slug` stays cheap to call from anywhere.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::env;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use caudra_config::providers::{
    AccountDef, ProviderDef, ProvidersConfig, QuotaWindowName, SwitchStrategy, SwitchingDef,
};
use caudra_storage::StateDir;
use caudra_storage::accounts::{
    AccountId, AccountIdentity, AccountName, AccountSelection, StoredAccount, StoredSecret,
    list_account_providers, list_accounts, persist_account_selection, read_account_selection,
    save_account_health,
};
use caudra_storage::auth::now_millis;
use tracing::warn;

use super::credential::{AccountCredential, AccountSecret, CredentialSource};
use super::health::{HealthBook, QuotaSource};
use super::policy::{Strategy, Thresholds, Unusable};
use super::quota::QuotaWindow;
use super::route::{AccountTier, SelectionMode};
use crate::AgentError;
use crate::model::Billing;
use crate::provider::{CredentialSpec, Provider, create_for_account, credential_spec};
use crate::providers::Timeouts;
use crate::types::ProviderUsage;

/// How often a cold decision re-reads config, stored credentials, and the
/// selection, so changes another process made reach this one.
const SNAPSHOT_REFRESH: Duration = Duration::from_secs(5);
const CONFIG_ACCOUNT: &str = "config";
const ENV_ACCOUNT: &str = "env";
const KEYLESS_ACCOUNT: &str = "default";
const KEYLESS_ALTERNATE: &str = "local";
const ENV_KEY_SEPARATOR: char = ',';
const OVERRIDE_SEPARATOR: char = ':';
const OVERRIDE_LIST_SEPARATOR: char = ',';

pub(crate) type ProviderFactory = Arc<
    dyn Fn(&AccountCredential, Timeouts) -> Result<Box<dyn Provider>, AgentError> + Send + Sync,
>;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AccountOverrideError {
    #[error("`{0}` must look like PROVIDER:ACCOUNT or PROVIDER:auto")]
    Shape(String),
    #[error("`{spec}`: {source}")]
    Name {
        spec: String,
        #[source]
        source: caudra_storage::accounts::AccountNameError,
    },
}

/// One account of a pool. `credential` is `None` for an account that is
/// listed but signed out.
#[derive(Clone, Debug)]
pub(crate) struct AccountSlot {
    pub name: AccountName,
    pub credential: Option<AccountCredential>,
    pub fallback: bool,
    pub configured: bool,
}

impl AccountSlot {
    pub(crate) fn billing(&self) -> Billing {
        self.credential
            .as_ref()
            .map_or(Billing::Api, AccountCredential::billing)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PoolSnapshot {
    pub accounts: Vec<AccountSlot>,
    pub selection: AccountSelection,
    pub overridden: bool,
    pub strategy: Strategy,
    pub thresholds: Thresholds,
    pub watched: Option<Vec<&'static str>>,
    /// The provider's `providers.toml` entry, hashed, so an edited base URL or
    /// header builds fresh provider instances.
    pub config_digest: u64,
}

impl PoolSnapshot {
    pub(crate) fn mode(&self) -> SelectionMode {
        match self.selection {
            AccountSelection::Auto => SelectionMode::Auto,
            AccountSelection::Pinned(_) => SelectionMode::Pinned,
        }
    }

    pub(crate) fn position(&self, name: &str) -> Option<usize> {
        self.accounts
            .iter()
            .position(|slot| slot.name.as_str() == name)
    }

    fn signed_in(&self) -> usize {
        self.accounts
            .iter()
            .filter(|slot| slot.credential.is_some())
            .count()
    }

    fn fingerprints(&self) -> Vec<(String, Option<String>)> {
        self.accounts
            .iter()
            .map(|slot| {
                (
                    slot.name.to_string(),
                    slot.credential.as_ref().map(AccountCredential::fingerprint),
                )
            })
            .collect()
    }
}

enum SnapshotSource {
    Live,
    #[cfg(test)]
    Fixed,
}

/// Why requests may not go to an account, beyond its quota.
#[derive(Default)]
struct Failures {
    auth: HashSet<String>,
    /// (account, model id)
    models: HashSet<(String, String)>,
}

/// (account, credential fingerprint, config digest, connect ms, stream ms)
type InstanceKey = (String, String, u64, u128, u128);

pub(crate) struct SlugState {
    pub slug: String,
    source: SnapshotSource,
    factory: ProviderFactory,
    storage: Option<StateDir>,
    snapshot: Mutex<(Arc<PoolSnapshot>, Instant)>,
    pub health: HealthBook,
    pub lineages: Mutex<super::router::Lineages>,
    instances: Mutex<HashMap<InstanceKey, Arc<dyn Provider>>>,
    failures: Mutex<Failures>,
}

impl SlugState {
    fn live(slug: &str) -> Result<Self, AgentError> {
        let storage = registry_storage();
        let snapshot = build_snapshot(slug, storage.as_ref())?;
        Ok(Self::new(
            slug,
            SnapshotSource::Live,
            Arc::new(create_for_account_in(slug)),
            storage,
            snapshot,
        ))
    }

    fn new(
        slug: &str,
        source: SnapshotSource,
        factory: ProviderFactory,
        storage: Option<StateDir>,
        snapshot: PoolSnapshot,
    ) -> Self {
        Self {
            slug: slug.to_owned(),
            source,
            factory,
            storage,
            snapshot: Mutex::new((Arc::new(snapshot), Instant::now())),
            health: HealthBook::new(slug),
            lineages: Mutex::new(super::router::Lineages::default()),
            instances: Mutex::new(HashMap::new()),
            failures: Mutex::new(Failures::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn fixed(slug: &str, factory: ProviderFactory, snapshot: PoolSnapshot) -> Self {
        Self::new(slug, SnapshotSource::Fixed, factory, None, snapshot)
    }

    #[cfg(test)]
    pub(crate) fn replace_snapshot(&self, snapshot: PoolSnapshot) {
        *self.snapshot.lock().unwrap() = (Arc::new(snapshot), Instant::now());
    }

    pub(crate) fn snapshot(&self) -> Arc<PoolSnapshot> {
        Arc::clone(&self.snapshot.lock().unwrap().0)
    }

    /// Re-reads the pool when it is older than the refresh interval, or
    /// always with `force`. Returns whether any credential changed.
    pub(crate) fn refresh(&self, force: bool) -> bool {
        if !matches!(self.source, SnapshotSource::Live) {
            return false;
        }
        let due = force || self.snapshot.lock().unwrap().1.elapsed() >= SNAPSHOT_REFRESH;
        if !due {
            return false;
        }
        let fresh = match build_snapshot(&self.slug, self.storage.as_ref()) {
            Ok(fresh) => fresh,
            Err(error) => {
                warn!(provider = %self.slug, %error, "keeping the previous account pool");
                self.snapshot.lock().unwrap().1 = Instant::now();
                return false;
            }
        };
        let mut current = self.snapshot.lock().unwrap();
        let changed = current.0.fingerprints() != fresh.fingerprints();
        *current = (Arc::new(fresh), Instant::now());
        changed
    }

    /// Quota readings are shared through storage only once there is more
    /// than one account to choose between.
    pub(crate) fn shares_health(&self) -> bool {
        self.snapshot().signed_in() > 1
    }

    pub(crate) fn reload_health(&self) {
        if let Some(storage) = self.storage.as_ref().filter(|_| self.shares_health()) {
            self.health.reload_if_due(storage);
        }
    }

    pub(crate) fn persist_health(
        &self,
        row: Option<caudra_storage::accounts::StoredAccountHealth>,
    ) {
        let (Some(row), Some(storage)) = (row, self.storage.clone()) else {
            return;
        };
        if !self.shares_health() {
            return;
        }
        smol::spawn(smol::unblock(move || {
            if let Err(error) = save_account_health(&storage, &row) {
                warn!(provider = %row.provider, account = %row.account, %error, "failed to store account health");
            }
        }))
        .detach();
    }

    pub(crate) fn instance(
        &self,
        credential: &AccountCredential,
        timeouts: Timeouts,
    ) -> Result<Arc<dyn Provider>, AgentError> {
        let key = (
            credential.id.name.to_string(),
            credential.fingerprint(),
            self.snapshot().config_digest,
            timeouts.connect.as_millis(),
            timeouts.stream.as_millis(),
        );
        if let Some(instance) = self.instances.lock().unwrap().get(&key) {
            return Ok(Arc::clone(instance));
        }
        let built: Arc<dyn Provider> = Arc::from((self.factory)(credential, timeouts)?);
        let mut instances = self.instances.lock().unwrap();
        instances.retain(|(name, fingerprint, digest, ..), _| {
            name != &key.0 || (fingerprint == &key.1 && *digest == key.2)
        });
        Ok(Arc::clone(instances.entry(key).or_insert(built)))
    }

    pub(crate) fn built_instances(&self) -> Vec<Arc<dyn Provider>> {
        self.instances.lock().unwrap().values().cloned().collect()
    }

    pub(crate) fn unusable(&self, slot: &AccountSlot, model_id: &str) -> Option<Unusable> {
        if slot.credential.is_none() {
            return Some(Unusable::SignedOut);
        }
        let failures = self.failures.lock().unwrap();
        let name = slot.name.as_str();
        if failures.auth.contains(name) {
            Some(Unusable::AuthFailed)
        } else if failures
            .models
            .contains(&(name.to_owned(), model_id.to_owned()))
        {
            Some(Unusable::ModelUnsupported)
        } else {
            None
        }
    }

    pub(crate) fn mark_auth_failed(&self, account: &str) {
        self.failures
            .lock()
            .unwrap()
            .auth
            .insert(account.to_owned());
    }

    pub(crate) fn mark_model_unsupported(&self, account: &str, model_id: &str) {
        self.failures
            .lock()
            .unwrap()
            .models
            .insert((account.to_owned(), model_id.to_owned()));
    }

    /// Lifts a refusal that left nowhere better to go, so the next request
    /// meets the provider's own error rather than an empty pool.
    pub(crate) fn forgive(&self, account: &str, model_id: &str) {
        let mut failures = self.failures.lock().unwrap();
        failures.auth.remove(account);
        failures
            .models
            .remove(&(account.to_owned(), model_id.to_owned()));
    }

    pub(crate) fn clear_auth_failures(&self) {
        self.failures.lock().unwrap().auth.clear();
    }

    pub(crate) fn now_ms(&self) -> u64 {
        now_millis()
    }
}

static POOLS: LazyLock<Mutex<HashMap<String, Arc<SlugState>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OVERRIDES: LazyLock<RwLock<HashMap<String, AccountSelection>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Unit tests never read or write the developer's real state directory.
fn registry_storage() -> Option<StateDir> {
    if cfg!(test) {
        None
    } else {
        StateDir::resolve().ok()
    }
}

fn create_for_account_in(
    slug: &str,
) -> impl Fn(&AccountCredential, Timeouts) -> Result<Box<dyn Provider>, AgentError> + Send + Sync + 'static
{
    let slug = slug.to_owned();
    move |credential, timeouts| create_for_account(&slug, credential, timeouts)
}

/// The slug's shared state, its pool re-read so a caller building a provider
/// sees what is configured and stored right now.
pub(crate) fn slug_state(slug: &str) -> Result<Arc<SlugState>, AgentError> {
    let existing = POOLS.lock().unwrap().get(slug).cloned();
    if let Some(state) = existing {
        state.refresh(true);
        return Ok(state);
    }
    let state = Arc::new(SlugState::live(slug)?);
    Ok(Arc::clone(
        POOLS
            .lock()
            .unwrap()
            .entry(slug.to_owned())
            .or_insert(state),
    ))
}

/// The providers `/accounts` lists: `current` first, then the order Auto
/// moves conversations across, then every other provider with a stored or
/// configured account.
pub fn account_providers(current: &str) -> Vec<String> {
    let config = ProvidersConfig::load();
    let stored = registry_storage()
        .map(|dir| {
            list_account_providers(&dir).unwrap_or_else(|error| {
                warn!(%error, "failed to list providers with stored accounts");
                Vec::new()
            })
        })
        .unwrap_or_default();
    let mut configured: Vec<&String> = config
        .providers
        .iter()
        .filter(|(_, definition)| !definition.accounts.is_empty())
        .map(|(slug, _)| slug)
        .collect();
    configured.sort();
    let mut providers = vec![current.to_owned()];
    for provider in config
        .provider_order()
        .iter()
        .chain(&stored)
        .chain(configured)
    {
        if !providers.contains(provider) {
            providers.push(provider.clone());
        }
    }
    providers
}

/// Another provider's state, for weighing a move to it: its pool and quota
/// readings as fresh as the refresh interval keeps them, and `None` when it
/// has no signed-in account. Blocks on storage, so callers run it unblocked.
pub(crate) fn peer_state(slug: &str) -> Option<Arc<SlugState>> {
    let existing = POOLS.lock().unwrap().get(slug).cloned();
    let state = match existing {
        Some(state) => {
            state.refresh(false);
            state
        }
        None => {
            let state = Arc::new(SlugState::live(slug).ok()?);
            Arc::clone(
                POOLS
                    .lock()
                    .unwrap()
                    .entry(slug.to_owned())
                    .or_insert(state),
            )
        }
    };
    state.reload_health();
    Some(state)
}

#[cfg(test)]
pub(crate) fn register_pool(state: Arc<SlugState>) {
    POOLS.lock().unwrap().insert(state.slug.clone(), state);
}

fn build_snapshot(slug: &str, storage: Option<&StateDir>) -> Result<PoolSnapshot, AgentError> {
    let spec = credential_spec(slug)?;
    let config = ProvidersConfig::load();
    let definition = config.get(slug);
    let stored = storage
        .map(|dir| {
            list_accounts(dir, slug).unwrap_or_else(|error| {
                warn!(provider = slug, %error, "failed to list stored accounts");
                Vec::new()
            })
        })
        .unwrap_or_default();
    let configured = definition.map_or(&[][..], |definition| definition.accounts.as_slice());
    let accounts = union(slug, &spec, configured, stored);
    let switching = definition
        .and_then(|definition| definition.switching.clone())
        .unwrap_or_default();
    let (selection, overridden) = match OVERRIDES.read().unwrap().get(slug) {
        Some(selection) => (selection.clone(), true),
        None => (
            storage.map_or(AccountSelection::Auto, |dir| {
                read_account_selection(dir, slug)
            }),
            false,
        ),
    };
    Ok(PoolSnapshot {
        accounts,
        selection,
        overridden,
        strategy: strategy(&switching),
        thresholds: thresholds(&switching),
        watched: switching.windows.as_ref().map(|windows| {
            windows
                .iter()
                .map(|window| QuotaWindowName::as_str(*window))
                .collect()
        }),
        config_digest: config_digest(definition),
    })
}

fn config_digest(definition: Option<&ProviderDef>) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(&definition)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn strategy(switching: &SwitchingDef) -> Strategy {
    match switching.strategy {
        SwitchStrategy::Ordered => Strategy::Ordered,
        SwitchStrategy::Paced => Strategy::Paced,
    }
}

fn thresholds(switching: &SwitchingDef) -> Thresholds {
    Thresholds {
        switch_below: f32::from(switching.switch_below),
        prefer_below: f32::from(switching.prefer_below),
    }
}

fn union(
    slug: &str,
    spec: &CredentialSpec,
    configured: &[AccountDef],
    mut stored: Vec<(AccountName, StoredAccount)>,
) -> Vec<AccountSlot> {
    let mut slots: Vec<AccountSlot> = Vec::new();
    let mut used_env = HashSet::new();
    for definition in configured {
        let Ok(name) = AccountName::new(definition.name.as_str()) else {
            continue;
        };
        let credential = match &definition.api_key_env {
            Some(var) => {
                used_env.insert(var.clone());
                env_keys(var).into_iter().next().map(|key| {
                    api_key_credential(slug, &name, key, CredentialSource::Env(var.clone()))
                })
            }
            None => stored
                .iter()
                .position(|(stored_name, _)| stored_name == &name)
                .map(|index| stored_credential(slug, stored.remove(index))),
        };
        if credential
            .as_ref()
            .is_some_and(|credential| definition.fallback && credential.is_oauth())
        {
            warn!(provider = slug, account = %name, "`fallback` only applies to API-key accounts; ignoring it");
        }
        slots.push(AccountSlot {
            fallback: definition.fallback,
            name,
            credential,
            configured: true,
        });
    }
    for (name, account) in stored {
        slots.push(AccountSlot {
            credential: Some(stored_credential(slug, (name.clone(), account))),
            name,
            fallback: false,
            configured: false,
        });
    }
    if let Some(key) = spec.config_key.clone() {
        push_implicit(
            &mut slots,
            slug,
            CONFIG_ACCOUNT,
            key,
            CredentialSource::Config,
        );
    }
    if let Some((var, keys)) = spec
        .env_vars
        .iter()
        .filter(|var| !used_env.contains(*var))
        .map(|var| (var, env_keys(var)))
        .find(|(_, keys)| !keys.is_empty())
    {
        let numbered = keys.len() > 1;
        for (position, key) in keys.into_iter().enumerate() {
            let name = if numbered {
                format!("{ENV_ACCOUNT}-{}", position + 1)
            } else {
                ENV_ACCOUNT.to_owned()
            };
            push_implicit(
                &mut slots,
                slug,
                &name,
                key,
                CredentialSource::Env(var.clone()),
            );
        }
    }
    if spec.keyless && slots.iter().all(|slot| slot.credential.is_none()) {
        let name = [KEYLESS_ACCOUNT, KEYLESS_ALTERNATE]
            .into_iter()
            .find(|name| slots.iter().all(|slot| slot.name.as_str() != *name))
            .and_then(|name| AccountName::new(name).ok());
        if let Some(name) = name {
            slots.push(AccountSlot {
                credential: Some(AccountCredential {
                    id: AccountId::new(slug, name.clone()),
                    secret: AccountSecret::Keyless,
                    source: CredentialSource::Config,
                    identity: AccountIdentity::default(),
                }),
                name,
                fallback: false,
                configured: false,
            });
        }
    }
    slots
}

fn push_implicit(
    slots: &mut Vec<AccountSlot>,
    slug: &str,
    name: &str,
    key: String,
    source: CredentialSource,
) {
    let Ok(name) = AccountName::new(name) else {
        return;
    };
    if slots.iter().any(|slot| slot.name == name) {
        return;
    }
    slots.push(AccountSlot {
        credential: Some(api_key_credential(slug, &name, key, source)),
        name,
        fallback: false,
        configured: false,
    });
}

fn env_keys(var: &str) -> Vec<String> {
    env::var(var)
        .map(|raw| {
            raw.split(ENV_KEY_SEPARATOR)
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn api_key_credential(
    slug: &str,
    name: &AccountName,
    key: String,
    source: CredentialSource,
) -> AccountCredential {
    AccountCredential {
        id: AccountId::new(slug, name.clone()),
        secret: AccountSecret::ApiKey { key, host: None },
        source,
        identity: AccountIdentity::default(),
    }
}

fn stored_credential(
    slug: &str,
    (name, account): (AccountName, StoredAccount),
) -> AccountCredential {
    AccountCredential {
        id: AccountId::new(slug, name),
        secret: match account.secret {
            StoredSecret::OAuth(tokens) => AccountSecret::OAuth(tokens),
            StoredSecret::ApiKey(credentials) => AccountSecret::ApiKey {
                key: credentials.api_key,
                host: credentials.host,
            },
        },
        source: CredentialSource::Stored,
        identity: account.identity,
    }
}

pub(crate) fn no_account_error(slug: &str) -> AgentError {
    let env_var = credential_spec(slug)
        .ok()
        .and_then(|spec| spec.env_vars.into_iter().next());
    let message = match env_var {
        Some(var) => format!(
            "{var} not set and no saved credentials for '{slug}' — run `caudra auth login {slug}`"
        ),
        None => format!("no saved credentials for '{slug}' — run `caudra auth login {slug}`"),
    };
    AgentError::Config { message }
}

/// `--account` and `CAUDRA_ACCOUNT`: `PROVIDER:NAME` or `PROVIDER:auto`, one
/// or several separated by commas.
pub fn parse_account_overrides(
    specs: &str,
) -> Result<Vec<(String, AccountSelection)>, AccountOverrideError> {
    specs
        .split(OVERRIDE_LIST_SEPARATOR)
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .map(parse_account_override)
        .collect()
}

pub fn parse_account_override(
    spec: &str,
) -> Result<(String, AccountSelection), AccountOverrideError> {
    let (provider, account) = spec
        .split_once(OVERRIDE_SEPARATOR)
        .filter(|(provider, account)| !provider.is_empty() && !account.is_empty())
        .ok_or_else(|| AccountOverrideError::Shape(spec.to_owned()))?;
    let selection = account
        .parse()
        .map_err(|source| AccountOverrideError::Name {
            spec: spec.to_owned(),
            source,
        })?;
    Ok((provider.to_owned(), selection))
}

/// A selection for this process only, which outranks the stored one and is
/// never written. `None` lifts it.
pub fn set_process_selection(provider: &str, selection: Option<AccountSelection>) {
    {
        let mut overrides = OVERRIDES.write().unwrap();
        match selection {
            Some(selection) => overrides.insert(provider.to_owned(), selection),
            None => overrides.remove(provider),
        };
    }
    invalidate(Some(provider));
}

/// Stores the user's choice and applies it to every routed provider of the
/// slug at its next request.
pub fn select_account(
    dir: &StateDir,
    provider: &str,
    selection: &AccountSelection,
) -> Result<(), AgentError> {
    persist_account_selection(dir, provider, selection).map_err(|error| AgentError::Config {
        message: format!("failed to save the {provider} account selection: {error}"),
    })?;
    invalidate(Some(provider));
    Ok(())
}

/// Makes the next request of the slug, or of every slug, re-read its pool:
/// after a login, a logout, a config change, or a selection.
pub fn invalidate(provider: Option<&str>) {
    let states: Vec<Arc<SlugState>> = {
        let pools = POOLS.lock().unwrap();
        match provider {
            Some(provider) => pools.get(provider).cloned().into_iter().collect(),
            None => pools.values().cloned().collect(),
        }
    };
    for state in states {
        state.refresh(true);
        state.clear_auth_failures();
    }
}

/// The account a subscription-only feature signs with: the pinned one when it
/// is a subscription, else the first signed-in subscription account.
pub fn oauth_account(provider: &str) -> Option<AccountCredential> {
    let snapshot = slug_state(provider).ok()?.snapshot();
    let oauth = |slot: &AccountSlot| slot.credential.clone().filter(AccountCredential::is_oauth);
    let pinned = match &snapshot.selection {
        AccountSelection::Pinned(name) => snapshot
            .accounts
            .iter()
            .find(|slot| &slot.name == name)
            .and_then(oauth),
        AccountSelection::Auto => None,
    };
    pinned.or_else(|| snapshot.accounts.iter().find_map(oauth))
}

/// Whether the slug has an account to sign with, as stored and configured
/// now, without building a provider. A keyless provider always has one. The
/// error names the `caudra auth login` to run.
pub fn require_account(provider: &str) -> Result<(), AgentError> {
    let snapshot = slug_state(provider)?.snapshot();
    if snapshot
        .accounts
        .iter()
        .any(|slot| slot.credential.is_some())
    {
        return Ok(());
    }
    Err(no_account_error(provider))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    Subscription,
    StoredKey,
    EnvKey,
    ConfigKey,
    Keyless,
    SignedOut,
}

/// One account as `/accounts` and `caudra auth status` list it.
#[derive(Debug, Clone)]
pub struct AccountStatus {
    pub name: String,
    pub kind: AccountKind,
    pub billing: Billing,
    pub fallback: bool,
    pub configured: bool,
    pub identity: AccountIdentity,
    pub masked_key: Option<String>,
    pub env_var: Option<String>,
    pub windows: Vec<QuotaWindow>,
    /// Epoch milliseconds of the latest quota reading, 0 when there is none.
    pub observed_at: u64,
    pub blocked_until: Option<u64>,
    pub tier: AccountTier,
    pub left_percent: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub provider: String,
    pub selection: AccountSelection,
    /// The selection comes from `--account` or `CAUDRA_ACCOUNT`.
    pub overridden: bool,
    pub strategy: SwitchStrategy,
    pub switch_below: u8,
    pub prefer_below: u8,
    pub accounts: Vec<AccountStatus>,
}

/// Where every account of the slug stands, as of the latest readings.
pub fn pool_status(provider: &str, model_id: Option<&str>) -> Result<PoolStatus, AgentError> {
    let state = slug_state(provider)?;
    if let Some(storage) = state.storage.as_ref() {
        state.health.reload_if_due(storage);
    }
    let snapshot = state.snapshot();
    let now_ms = now_millis();
    let model_id = model_id.unwrap_or_default();
    let accounts = snapshot
        .accounts
        .iter()
        .map(|slot| {
            let health = state.health.get(slot.name.as_str());
            let candidate = super::policy::Candidate {
                name: slot.name.as_str(),
                billing: slot.billing(),
                fallback: slot.fallback,
                unusable: state.unusable(slot, model_id),
                windows: &health.windows,
                blocked_until: health.blocked_until,
            };
            let assessment = super::policy::assess(
                &candidate,
                snapshot.thresholds,
                snapshot.watched.as_deref(),
                model_id,
                now_ms,
            );
            AccountStatus {
                name: slot.name.to_string(),
                kind: account_kind(slot),
                billing: slot.billing(),
                fallback: slot.fallback,
                configured: slot.configured,
                identity: slot
                    .credential
                    .as_ref()
                    .map(|credential| credential.identity.clone())
                    .unwrap_or_default(),
                masked_key: slot
                    .credential
                    .as_ref()
                    .and_then(AccountCredential::masked_key),
                env_var: slot
                    .credential
                    .as_ref()
                    .and_then(|credential| match &credential.source {
                        CredentialSource::Env(var) => Some(var.clone()),
                        CredentialSource::Stored | CredentialSource::Config => None,
                    }),
                windows: health.windows,
                observed_at: health.observed_at,
                blocked_until: health.blocked_until.filter(|until| *until > now_ms),
                tier: assessment.tier,
                left_percent: assessment.left.map(percent),
            }
        })
        .collect();
    let switching = ProvidersConfig::load()
        .get(provider)
        .and_then(|definition| definition.switching.clone())
        .unwrap_or_default();
    Ok(PoolStatus {
        provider: provider.to_owned(),
        selection: snapshot.selection.clone(),
        overridden: snapshot.overridden,
        strategy: switching.strategy,
        switch_below: switching.switch_below,
        prefer_below: switching.prefer_below,
        accounts,
    })
}

fn account_kind(slot: &AccountSlot) -> AccountKind {
    match slot.credential.as_ref() {
        None => AccountKind::SignedOut,
        Some(credential) => match (&credential.secret, &credential.source) {
            (AccountSecret::OAuth(_), _) => AccountKind::Subscription,
            (AccountSecret::Keyless, _) => AccountKind::Keyless,
            (AccountSecret::ApiKey { .. }, CredentialSource::Stored) => AccountKind::StoredKey,
            (AccountSecret::ApiKey { .. }, CredentialSource::Env(_)) => AccountKind::EnvKey,
            (AccountSecret::ApiKey { .. }, CredentialSource::Config) => AccountKind::ConfigKey,
        },
    }
}

pub(crate) fn percent(left: f32) -> u8 {
    left.round().clamp(0.0, 100.0) as u8
}

/// Asks one account's provider for its quota, and records the windows it
/// reports so switching sees them.
pub async fn fetch_account_usage(
    provider: &str,
    account: &str,
    timeouts: Timeouts,
) -> Result<Option<ProviderUsage>, AgentError> {
    let state = slug_state(provider)?;
    let snapshot = state.snapshot();
    let credential = snapshot
        .position(account)
        .and_then(|index| snapshot.accounts[index].credential.clone())
        .ok_or_else(|| AgentError::Config {
            message: format!("{provider} has no signed-in account named `{account}`"),
        })?;
    let instance = state.instance(&credential, timeouts)?;
    let usage = instance.fetch_usage().await?;
    if let Some(usage) = &usage {
        let windows: Vec<QuotaWindow> = usage
            .limits
            .iter()
            .filter_map(crate::types::UsageLimit::quota_window)
            .collect();
        let row = state
            .health
            .observe(account, QuotaSource::Usage, windows, now_millis());
        state.persist_health(row);
    }
    Ok(usage)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn slot(credential: AccountCredential, fallback: bool) -> AccountSlot {
        AccountSlot {
            name: credential.id.name.clone(),
            credential: Some(credential),
            fallback,
            configured: true,
        }
    }

    pub(crate) fn snapshot(
        accounts: Vec<AccountSlot>,
        selection: AccountSelection,
    ) -> PoolSnapshot {
        PoolSnapshot {
            accounts,
            selection,
            overridden: false,
            strategy: Strategy::Ordered,
            thresholds: thresholds(&SwitchingDef::default()),
            watched: None,
            config_digest: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const SLUG: &str = "test-provider";
    const KEYLESS_SLUG: &str = "ollama";

    fn spec(env_vars: &[&str], keyless: bool, config_key: Option<&str>) -> CredentialSpec {
        CredentialSpec {
            env_vars: env_vars.iter().map(|var| (*var).to_owned()).collect(),
            keyless,
            oauth: false,
            config_key: config_key.map(str::to_owned),
        }
    }

    fn definition(name: &str, api_key_env: Option<&str>) -> AccountDef {
        AccountDef {
            name: name.to_owned(),
            api_key_env: api_key_env.map(str::to_owned),
            fallback: false,
        }
    }

    fn stored(name: &str, key: &str, created_at: u64) -> (AccountName, StoredAccount) {
        (
            AccountName::new(name).unwrap(),
            StoredAccount {
                secret: StoredSecret::ApiKey(caudra_storage::auth::ProviderCredentials {
                    api_key: key.to_owned(),
                    host: None,
                }),
                identity: AccountIdentity::default(),
                created_at,
            },
        )
    }

    fn names(slots: &[AccountSlot]) -> Vec<(String, bool)> {
        slots
            .iter()
            .map(|slot| (slot.name.to_string(), slot.credential.is_some()))
            .collect()
    }

    fn unique_var(label: &str) -> String {
        format!("CAUDRA_TEST_ACCOUNTS_{label}_{}", fastrand::u32(..))
    }

    #[test]
    fn configured_accounts_lead_and_stored_ones_follow() {
        let slots = union(
            SLUG,
            &spec(&[], false, None),
            &[definition("team", None), definition("missing", None)],
            vec![stored("personal", "sk-1", 1), stored("team", "sk-2", 2)],
        );
        assert_eq!(
            names(&slots),
            vec![
                ("team".into(), true),
                ("missing".into(), false),
                ("personal".into(), true),
            ]
        );
    }

    #[test]
    fn config_and_env_keys_come_last() {
        let var = unique_var("ENV");
        unsafe { env::set_var(&var, "sk-a, sk-b") };
        let slots = union(
            SLUG,
            &spec(&[&var], false, Some("sk-config")),
            &[],
            Vec::new(),
        );
        unsafe { env::remove_var(&var) };
        assert_eq!(
            names(&slots),
            vec![
                ("config".into(), true),
                ("env-1".into(), true),
                ("env-2".into(), true),
            ]
        );
    }

    #[test]
    fn an_env_var_an_entry_names_is_not_listed_twice() {
        let var = unique_var("CLAIMED");
        unsafe { env::set_var(&var, "sk-a") };
        let slots = union(
            SLUG,
            &spec(&[&var], false, None),
            &[definition("api", Some(&var))],
            Vec::new(),
        );
        unsafe { env::remove_var(&var) };
        assert_eq!(names(&slots), vec![("api".into(), true)]);
    }

    #[test_case(true, vec![("default".into(), true)] ; "keyless_provider")]
    #[test_case(false, vec![] ; "keyed_provider")]
    fn keyless_providers_get_one_account(keyless: bool, expected: Vec<(String, bool)>) {
        let slots = union(SLUG, &spec(&[], keyless, None), &[], Vec::new());
        assert_eq!(names(&slots), expected);
    }

    #[test]
    fn a_keyless_provider_always_has_an_account() {
        assert!(require_account(KEYLESS_SLUG).is_ok());
    }

    #[test_case("anthropic:team", "anthropic", "team" ; "pinned")]
    #[test_case("openai:auto", "openai", "auto" ; "auto")]
    fn overrides_parse(spec: &str, provider: &str, selection: &str) {
        let parsed = parse_account_overrides(spec).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, provider);
        assert_eq!(parsed[0].1.to_string(), selection);
    }

    #[test]
    fn overrides_take_lists() {
        assert_eq!(
            parse_account_overrides("anthropic:team, openai:auto")
                .unwrap()
                .len(),
            2
        );
    }

    #[test_case("anthropic" ; "no_separator")]
    #[test_case(":team" ; "no_provider")]
    #[test_case("anthropic:" ; "no_account")]
    fn malformed_overrides_are_refused(spec: &str) {
        assert_eq!(
            parse_account_overrides(spec),
            Err(AccountOverrideError::Shape(spec.to_owned()))
        );
    }
}
