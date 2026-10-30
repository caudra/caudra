//! Changes the user makes to a provider's accounts, shared by `caudra auth`
//! and `/accounts`. Each keeps the stored credential, its quota readings, the
//! `providers.toml` entry, and the saved selection in step, then makes every
//! routed provider of the slug re-read its pool.

use caudra_config::providers::{
    AccountDef, MIN_PROVIDER_ORDER, ProvidersFile, ProvidersFileError, SwitchStrategy, SwitchingDef,
};
use caudra_storage::StateDir;
use caudra_storage::accounts::{
    AccountId, AccountName, AccountSelection, delete_account, delete_account_health, load_account,
    persist_account_selection, read_account_selection, rename_account_health,
};

use super::registry::{AccountKind, AccountStatus, follow_rename, invalidate, pool_status};
use crate::AgentError;

/// What signing out of an account undid besides its credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedOut {
    /// A credential was stored for it.
    pub had_credential: bool,
    /// It was the pinned account, so the provider is back on Auto.
    pub unpinned: bool,
}

/// Renames the stored credential, its quota readings, its `providers.toml`
/// entry, and the pin on it, whichever of them exist. Conversations on it
/// stay on it.
pub fn rename_account(
    dir: &StateDir,
    provider: &str,
    from: &AccountName,
    to: &AccountName,
) -> Result<(), AgentError> {
    let mut file = ProvidersFile::load().map_err(config_error)?;
    let configured = file.get(provider).is_some_and(|definition| {
        definition
            .accounts
            .iter()
            .any(|account| account.name == from.as_str())
    });
    let stored = load_account(dir, &AccountId::new(provider, from.clone()))?.is_some();
    if !configured && !stored {
        return Err(missing(provider, from));
    }
    if stored {
        caudra_storage::accounts::rename_account(dir, provider, from, to)?;
        rename_account_health(dir, provider, from.as_str(), to.as_str())?;
    }
    if configured {
        file.rename_account(provider, from.as_str(), to.as_str())
            .and_then(|()| file.save())
            .map_err(config_error)?;
    }
    if read_account_selection(dir, provider) == AccountSelection::Pinned(from.clone()) {
        persist_account_selection(dir, provider, &AccountSelection::Pinned(to.clone()))?;
    }
    follow_rename(provider, from, to);
    invalidate(Some(provider));
    Ok(())
}

/// Deletes the stored credential and quota readings of `name`, leaving any
/// `providers.toml` entry to list it as signed out. A pin on it falls back
/// to Auto, since requests could only fail while it stood.
pub fn sign_out(
    dir: &StateDir,
    provider: &str,
    name: &AccountName,
) -> Result<SignedOut, AgentError> {
    let had_credential = delete_account(dir, &AccountId::new(provider, name.clone()))?;
    delete_account_health(dir, provider, name.as_str())?;
    let unpinned = read_account_selection(dir, provider) == AccountSelection::Pinned(name.clone());
    if unpinned {
        persist_account_selection(dir, provider, &AccountSelection::Auto)?;
    }
    invalidate(Some(provider));
    Ok(SignedOut {
        had_credential,
        unpinned,
    })
}

/// [`sign_out`], and the `providers.toml` entry goes too.
pub fn forget_account(
    dir: &StateDir,
    provider: &str,
    name: &AccountName,
) -> Result<SignedOut, AgentError> {
    let mut file = ProvidersFile::load().map_err(config_error)?;
    if file.remove_account(provider, name.as_str()) {
        file.save().map_err(config_error)?;
    }
    sign_out(dir, provider, name)
}

/// Moves `name` by `offset` places in the order Auto tries the accounts,
/// towards the front when negative. That order lives in `providers.toml`, so
/// the saved accounts it does not list yet join it first, in the order they
/// are tried now. False when the account was already at that end.
pub fn move_account(provider: &str, name: &AccountName, offset: isize) -> Result<bool, AgentError> {
    let pool = pool_status(provider, None)?;
    let account = pool
        .accounts
        .iter()
        .find(|account| account.name == name.as_str())
        .ok_or_else(|| missing(provider, name))?;
    if !orderable(account) {
        return Err(AgentError::Config {
            message: format!(
                "{provider} account `{name}` is not a saved account, so it is always tried after them"
            ),
        });
    }
    let mut file = ProvidersFile::load().map_err(config_error)?;
    for account in pool
        .accounts
        .iter()
        .filter(|account| orderable(account) && !account.configured)
    {
        let entry = AccountDef {
            name: account.name.clone(),
            api_key_env: None,
            fallback: false,
        };
        file.upsert_account(provider, &entry)
            .map_err(config_error)?;
    }
    let moved = file.move_account(provider, name.as_str(), offset);
    if moved {
        file.save().map_err(config_error)?;
        invalidate(Some(provider));
    }
    Ok(moved)
}

/// Keeps the thresholds and windows `[SLUG.switching]` already sets.
pub fn set_strategy(provider: &str, strategy: SwitchStrategy) -> Result<(), AgentError> {
    let mut file = ProvidersFile::load().map_err(config_error)?;
    let switching = SwitchingDef {
        strategy,
        ..file
            .get(provider)
            .and_then(|definition| definition.switching.clone())
            .unwrap_or_default()
    };
    file.set_switching(provider, &switching)
        .and_then(|()| file.save())
        .map_err(config_error)?;
    invalidate(Some(provider));
    Ok(())
}

/// The providers Auto moves a conversation across, first to last. Fewer
/// than two drops the order, which keeps every conversation on its provider.
pub fn set_provider_order(providers: &[String]) -> Result<(), AgentError> {
    let providers: &[String] = match providers.len() < MIN_PROVIDER_ORDER {
        true => &[],
        false => providers,
    };
    let mut file = ProvidersFile::load().map_err(config_error)?;
    file.set_provider_order(providers)
        .and_then(|()| file.save())
        .map_err(config_error)
}

/// What `providers.toml` can list: its own entries and the credentials
/// `caudra auth login` saved, not the provider key or its variable.
fn orderable(account: &AccountStatus) -> bool {
    account.configured
        || matches!(
            account.kind,
            AccountKind::Subscription | AccountKind::StoredKey
        )
}

fn missing(provider: &str, name: &AccountName) -> AgentError {
    AgentError::Config {
        message: format!("{provider} has no account named `{name}`"),
    }
}

fn config_error(error: ProvidersFileError) -> AgentError {
    AgentError::Config {
        message: error.to_string(),
    }
}
