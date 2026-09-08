use std::thread;
use std::time::{Duration, Instant};

use caudra_storage::StateDir;
use caudra_storage::auth::{OAuthTokens, save_tokens, try_load_tokens, try_lock_provider_auth};
use tracing::warn;

use crate::AgentError;

const LOCK_POLL_MIN_MS: u64 = 25;
const LOCK_POLL_MAX_MS: u64 = 100;
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy)]
pub(crate) enum RefreshReason<'a> {
    Proactive,
    Rejected(&'a [String]),
}

pub(crate) fn refresh_from_storage(
    storage: &StateDir,
    provider: &str,
    reason: RefreshReason<'_>,
    refresh: impl Fn(&OAuthTokens) -> Result<OAuthTokens, AgentError>,
) -> Result<OAuthTokens, AgentError> {
    if let Some(tokens) = load_tokens(storage, provider)?
        && usable_without_refresh(&tokens, reason)
    {
        return Ok(tokens);
    }

    let started = Instant::now();
    loop {
        let Some(_lock) = try_lock_provider_auth(storage, provider)? else {
            if started.elapsed() >= LOCK_WAIT_TIMEOUT {
                return Err(AgentError::Config {
                    message: format!("timed out waiting for another {provider} OAuth refresh"),
                });
            }
            thread::sleep(Duration::from_millis(fastrand::u64(
                LOCK_POLL_MIN_MS..=LOCK_POLL_MAX_MS,
            )));
            if let Some(tokens) = load_tokens(storage, provider)?
                && usable_without_refresh(&tokens, reason)
            {
                return Ok(tokens);
            }
            continue;
        };

        let tokens = load_tokens(storage, provider)?.ok_or_else(|| {
            AgentError::api(401, format!("{provider} OAuth tokens not found on disk"))
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
                warn!(provider, %error, "OAuth refresh failed; using still-valid access token");
                return Ok(tokens);
            }
            Err(error) => return Err(error),
        };
        if matches!(reason, RefreshReason::Rejected(rejected) if rejected.contains(&fresh.access)) {
            return Err(AgentError::api(
                401,
                format!("{provider} OAuth refresh returned the rejected access token"),
            ));
        }
        save_tokens(storage, provider, &fresh)?;
        return Ok(fresh);
    }
}

fn load_tokens(storage: &StateDir, provider: &str) -> Result<Option<OAuthTokens>, AgentError> {
    try_load_tokens(storage, provider).map_err(|error| AgentError::Config {
        message: format!("failed to read {provider} OAuth credentials: {error}"),
    })
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

    use caudra_storage::auth::{now_millis, save_tokens};
    use tempfile::TempDir;

    use super::*;

    const PROVIDER: &str = "test-oauth";

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
        save_tokens(&storage, PROVIDER, &tokens("old", "old-refresh", 0)).unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let storage = storage.clone();
                let refreshes = Arc::clone(&refreshes);
                std::thread::spawn(move || {
                    refresh_from_storage(&storage, PROVIDER, RefreshReason::Proactive, |_| {
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
        save_tokens(&storage, PROVIDER, &current).unwrap();

        let resolved = refresh_from_storage(&storage, PROVIDER, RefreshReason::Proactive, |_| {
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
            PROVIDER,
            &tokens(
                "rejected",
                "refresh",
                now_millis().saturating_add(3_600_000),
            ),
        )
        .unwrap();

        let rejected = vec!["rejected".into()];
        let error = refresh_from_storage(
            &storage,
            PROVIDER,
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
            PROVIDER,
            &tokens(
                "replacement",
                "replacement-refresh",
                now_millis().saturating_add(3_600_000),
            ),
        )
        .unwrap();

        let rejected = vec!["rejected".into()];
        let resolved = refresh_from_storage(
            &storage,
            PROVIDER,
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
        save_tokens(&storage, PROVIDER, &current).unwrap();
        let rejected = vec!["rejected".into()];

        let error = refresh_from_storage(
            &storage,
            PROVIDER,
            RefreshReason::Rejected(&rejected),
            |_| Ok(tokens("rejected", "replacement-refresh", u64::MAX)),
        )
        .unwrap_err();

        assert!(matches!(error, AgentError::Api { status: 401, .. }));
        assert_eq!(load_tokens(&storage, PROVIDER).unwrap(), Some(current));
    }
}
