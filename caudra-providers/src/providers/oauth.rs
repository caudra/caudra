use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use caudra_storage::StateDir;
use caudra_storage::accounts::{
    AccountId, StoredSecret, load_account, save_account_tokens_locked, try_lock_account,
};
use caudra_storage::auth::OAuthTokens;
use serde_json::Value;
use tracing::warn;

use crate::AgentError;
use crate::accounts::{AccountCredential, AccountSecret, CredentialSource};

const LOCK_POLL_MIN_MS: u64 = 25;
const LOCK_POLL_MAX_MS: u64 = 100;
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(45);
const JWT_PARTS: usize = 3;

#[derive(Clone, Copy)]
pub(crate) enum RefreshReason<'a> {
    Proactive,
    Rejected(&'a [String]),
}

impl<'a> RefreshReason<'a> {
    /// A refresh with nothing rejected yet only renews a token near expiry.
    pub(crate) fn for_rejected(rejected: &'a [String]) -> Self {
        if rejected.is_empty() {
            Self::Proactive
        } else {
            Self::Rejected(rejected)
        }
    }
}

/// Where an account read from storage lives, so the provider signing in as
/// it can refresh its tokens and re-read what a later login replaced.
#[derive(Clone)]
pub(crate) struct AccountStore {
    storage: StateDir,
    id: AccountId,
}

impl AccountStore {
    pub(crate) fn new(storage: StateDir, id: AccountId) -> Self {
        Self { storage, id }
    }

    /// `None` for a credential from the environment or `providers.toml`,
    /// which Caudra can neither refresh nor re-read.
    pub(crate) fn for_account(account: &AccountCredential) -> Result<Option<Self>, AgentError> {
        if account.source != CredentialSource::Stored {
            return Ok(None);
        }
        Ok(Some(Self::new(StateDir::resolve()?, account.id.clone())))
    }

    pub(crate) fn refresh(
        &self,
        reason: RefreshReason<'_>,
        refresh: impl Fn(&OAuthTokens) -> Result<OAuthTokens, AgentError>,
    ) -> Result<OAuthTokens, AgentError> {
        refresh_from_storage(&self.storage, &self.id, reason, refresh)
    }

    pub(crate) fn reload(&self) -> Result<AccountSecret, AgentError> {
        load_account(&self.storage, &self.id)?
            .map(|account| account.secret.into())
            .ok_or_else(|| AgentError::Config {
                message: format!(
                    "{} is signed out; run `caudra auth login {}`",
                    self.id, self.id.provider
                ),
            })
    }
}

pub(crate) fn refresh_from_storage(
    storage: &StateDir,
    account: &AccountId,
    reason: RefreshReason<'_>,
    refresh: impl Fn(&OAuthTokens) -> Result<OAuthTokens, AgentError>,
) -> Result<OAuthTokens, AgentError> {
    if let Some(tokens) = load_tokens(storage, account)?
        && usable_without_refresh(&tokens, reason)
    {
        return Ok(tokens);
    }

    let started = Instant::now();
    loop {
        let Some(_lock) = try_lock_account(storage, account)? else {
            if started.elapsed() >= LOCK_WAIT_TIMEOUT {
                return Err(AgentError::Config {
                    message: format!("timed out waiting for another {account} OAuth refresh"),
                });
            }
            thread::sleep(Duration::from_millis(fastrand::u64(
                LOCK_POLL_MIN_MS..=LOCK_POLL_MAX_MS,
            )));
            if let Some(tokens) = load_tokens(storage, account)?
                && usable_without_refresh(&tokens, reason)
            {
                return Ok(tokens);
            }
            continue;
        };

        let tokens = load_tokens(storage, account)?.ok_or_else(|| {
            AgentError::api(401, format!("{account} OAuth tokens not found on disk"))
        })?;
        if usable_without_refresh(&tokens, reason) {
            return Ok(tokens);
        }

        let fresh = match refresh(&tokens) {
            Ok(fresh) => fresh,
            Err(error)
                if matches!(reason, RefreshReason::Proactive)
                    && !tokens.is_hard_expired()
                    && error.is_retryable() =>
            {
                warn!(%account, %error, "OAuth refresh failed; using still-valid access token");
                return Ok(tokens);
            }
            Err(error) => return Err(error),
        };
        if matches!(reason, RefreshReason::Rejected(rejected) if rejected.contains(&fresh.access)) {
            return Err(AgentError::api(
                401,
                format!("{account} OAuth refresh returned the rejected access token"),
            ));
        }
        save_account_tokens_locked(storage, account, &fresh)?;
        return Ok(fresh);
    }
}

/// The unverified payload of a JWT, for naming the account a login signed in
/// as. Never an authorization decision.
pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != JWT_PARTS {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice(&payload).ok()
}

#[cfg(test)]
pub(crate) fn unsigned_jwt(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(b"{}");
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    format!("{header}.{payload}.sig")
}

fn load_tokens(storage: &StateDir, account: &AccountId) -> Result<Option<OAuthTokens>, AgentError> {
    let stored = load_account(storage, account).map_err(|error| AgentError::Config {
        message: format!("failed to read {account} OAuth credentials: {error}"),
    })?;
    Ok(stored.and_then(|stored| match stored.secret {
        StoredSecret::OAuth(tokens) => Some(tokens),
        StoredSecret::ApiKey(_) => None,
    }))
}

fn usable_without_refresh(tokens: &OAuthTokens, reason: RefreshReason<'_>) -> bool {
    match reason {
        RefreshReason::Proactive => !tokens.is_expired(),
        RefreshReason::Rejected(rejected) => {
            !rejected.contains(&tokens.access) && !tokens.is_hard_expired()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use caudra_storage::accounts::{AccountIdentity, AccountName, StoredAccount, save_account};
    use caudra_storage::auth::now_millis;
    use tempfile::TempDir;

    use super::*;

    const PROVIDER: &str = "test-oauth";
    const ACCOUNT: &str = "personal";
    const CREATED_AT: u64 = 1_767_225_600_000;

    fn account() -> AccountId {
        AccountId::new(PROVIDER, AccountName::new(ACCOUNT).unwrap())
    }

    fn save_tokens(storage: &StateDir, tokens: &OAuthTokens) {
        save_account(
            storage,
            &account(),
            &StoredAccount {
                secret: StoredSecret::OAuth(tokens.clone()),
                identity: AccountIdentity::default(),
                created_at: CREATED_AT,
            },
        )
        .unwrap();
    }

    fn storage() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        (temp, storage)
    }

    fn tokens(access: &str, refresh: &str, expires: u64) -> OAuthTokens {
        OAuthTokens {
            access: access.into(),
            refresh: refresh.into(),
            expires,
            account_id: None,
        }
    }

    #[test]
    fn concurrent_refreshers_share_one_rotation() {
        let (_temp, storage) = storage();
        save_tokens(&storage, &tokens("old", "old-refresh", 0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let storage = storage.clone();
                let refreshes = Arc::clone(&refreshes);
                std::thread::spawn(move || {
                    refresh_from_storage(&storage, &account(), RefreshReason::Proactive, |_| {
                        refreshes.fetch_add(1, Ordering::SeqCst);
                        Ok(tokens(
                            "new",
                            "new-refresh",
                            now_millis().saturating_add(3_600_000),
                        ))
                    })
                    .unwrap()
                })
            })
            .collect();
        let refreshed: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(refreshed.iter().all(|tokens| tokens.access == "new"));
        assert!(
            refreshed
                .iter()
                .all(|tokens| tokens.refresh == "new-refresh")
        );
    }

    #[test]
    fn proactive_retryable_failure_uses_hard_valid_access() {
        let (_temp, storage) = storage();
        let current = tokens("access", "refresh", now_millis().saturating_add(30_000));
        save_tokens(&storage, &current);

        let resolved = refresh_from_storage(&storage, &account(), RefreshReason::Proactive, |_| {
            Err(AgentError::api(500, "temporary"))
        })
        .unwrap();

        assert_eq!(resolved.access, current.access);
    }

    #[test]
    fn forced_refresh_does_not_fallback_to_rejected_access() {
        let (_temp, storage) = storage();
        save_tokens(
            &storage,
            &tokens(
                "rejected",
                "refresh",
                now_millis().saturating_add(3_600_000),
            ),
        );

        let rejected = vec!["rejected".into()];
        let error = refresh_from_storage(
            &storage,
            &account(),
            RefreshReason::Rejected(&rejected),
            |_| Err(AgentError::api(500, "temporary")),
        )
        .unwrap_err();

        assert!(matches!(error, AgentError::Api { status: 500, .. }));
    }

    #[test]
    fn changed_access_is_adopted_without_refresh() {
        let (_temp, storage) = storage();
        save_tokens(
            &storage,
            &tokens(
                "replacement",
                "replacement-refresh",
                now_millis().saturating_add(3_600_000),
            ),
        );

        let rejected = vec!["rejected".into()];
        let resolved = refresh_from_storage(
            &storage,
            &account(),
            RefreshReason::Rejected(&rejected),
            |_| panic!("replacement credentials must not be refreshed"),
        )
        .unwrap();

        assert_eq!(resolved.access, "replacement");
    }

    #[test]
    fn rejected_refresh_response_is_not_persisted() {
        let (_temp, storage) = storage();
        let current = tokens("rejected", "current-refresh", 0);
        save_tokens(&storage, &current);
        let rejected = vec!["rejected".into()];

        let error = refresh_from_storage(
            &storage,
            &account(),
            RefreshReason::Rejected(&rejected),
            |_| Ok(tokens("rejected", "replacement-refresh", u64::MAX)),
        )
        .unwrap_err();

        assert!(matches!(error, AgentError::Api { status: 401, .. }));
        assert_eq!(load_tokens(&storage, &account()).unwrap(), Some(current));
    }
}
