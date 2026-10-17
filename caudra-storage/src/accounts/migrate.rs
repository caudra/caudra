//! Moves the single-account layout, one `auth/<provider>.json` per provider,
//! into the named layout as each provider's `default` account.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use tracing::{info, warn};

use super::credentials::{
    LOCK_EXTENSION, RECORD_EXTENSION, account_exists, lock_account, write_account_locked,
};
use super::{
    AccountId, AccountIdentity, AccountName, DEFAULT_ACCOUNT, StoredAccount, StoredSecret,
    validate_provider_slug,
};
use crate::auth::{
    AUTH_DIR, AUTH_FILE_MODE, ProviderAuth, now_millis, try_load_provider_auth_path,
};
use crate::{StateDir, StorageError, exclusive_state_lock};

const MCP_PREFIX: &str = "mcp-";

/// Persistent roots this process already migrated.
static MIGRATED_ROOTS: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Runs [`migrate_legacy_auth`] once per persistent root for the life of the
/// process. A caller that arrives while another migrates waits for it, so
/// nobody reads accounts from a half-moved layout. A failure is logged rather
/// than returned: the old files stay where they were and the next process
/// tries again.
pub fn ensure_legacy_auth_migrated(dir: &StateDir) {
    let mut migrated_roots = MIGRATED_ROOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !migrated_roots.insert(dir.persistent_path().to_path_buf()) {
        return;
    }
    match migrate_legacy_auth(dir) {
        Ok(providers) if providers.is_empty() => {}
        Ok(providers) => {
            info!(
                ?providers,
                "moved single-account credentials to each provider's default account"
            );
        }
        Err(error) => warn!(
            %error,
            root = %dir.persistent_path().display(),
            "could not move single-account credentials to named accounts; \
             sign in again if a provider reports no credential"
        ),
    }
}

/// Moves every provider credential file to `auth/accounts/<provider>/default.json`
/// and returns the providers moved. MCP credentials and anything that does not
/// read as a provider credential stay where they are. Safe to rerun, including
/// after a crash between writing the new file and removing the old one: an
/// existing `default` account is kept and only the old file goes.
pub fn migrate_legacy_auth(dir: &StateDir) -> Result<Vec<String>, StorageError> {
    let auth_dir = dir.persistent_path().join(AUTH_DIR);
    let mut migrated = Vec::new();
    for provider in legacy_providers(&auth_dir)? {
        if migrate_provider(dir, &auth_dir, &provider)? {
            migrated.push(provider);
        }
    }
    Ok(migrated)
}

fn legacy_providers(auth_dir: &Path) -> Result<Vec<String>, StorageError> {
    let entries = match fs::read_dir(auth_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut providers = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || path.extension() != Some(OsStr::new(RECORD_EXTENSION)) {
            continue;
        }
        let Some(provider) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        if !provider.starts_with(MCP_PREFIX) && validate_provider_slug(provider).is_ok() {
            providers.push(provider.to_owned());
        }
    }
    providers.sort();
    Ok(providers)
}

fn migrate_provider(dir: &StateDir, auth_dir: &Path, provider: &str) -> Result<bool, StorageError> {
    let legacy = auth_dir.join(format!("{provider}.{RECORD_EXTENSION}"));
    let legacy_lock_path = auth_dir.join(format!("{provider}.{LOCK_EXTENSION}"));
    let legacy_lock = exclusive_state_lock(&legacy_lock_path, AUTH_FILE_MODE)?;
    let secret = match try_load_provider_auth_path(&legacy) {
        Ok(Some(ProviderAuth::OAuth(tokens))) => StoredSecret::OAuth(tokens),
        Ok(Some(ProviderAuth::ApiKey(credentials))) => StoredSecret::ApiKey(credentials),
        Ok(None) => return Ok(false),
        Err(error) => {
            warn!(
                path = %legacy.display(),
                %error,
                "leaving an auth file that does not read as a provider credential"
            );
            return Ok(false);
        }
    };
    let id = AccountId::new(provider, AccountName::new(DEFAULT_ACCOUNT)?);
    {
        let _account_lock = lock_account(dir, &id)?;
        if !account_exists(dir, &id)? {
            let account = StoredAccount {
                secret,
                identity: AccountIdentity::default(),
                created_at: modified_millis(&legacy),
            };
            write_account_locked(dir, &id, &account)?;
        }
    }
    fs::remove_file(&legacy)?;
    drop(legacy_lock);
    match fs::remove_file(&legacy_lock_path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(true),
    }
}

/// When the old file was last written, which is the closest record of when
/// the account was added.
fn modified_millis(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or_else(now_millis, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::accounts::{list_accounts, load_account, save_account};
    use crate::auth::{OAuthTokens, ProviderCredentials, save_auth};

    const ANTHROPIC: &str = "anthropic";
    const OPENAI: &str = "openai";
    const MCP_FILE: &str = "mcp-server";
    const NOT_A_CREDENTIAL: &str = "notes";
    const ACCESS: &str = "access-token";
    const REFRESH: &str = "refresh-token";
    const EXPIRES: u64 = 9_999_999_999_999;
    const OPENAI_ACCOUNT: &str = "acct_123";
    const API_KEY: &str = "sk-test-key";
    const NEWER_KEY: &str = "sk-newer-key";
    const MODIFIED_MS: u64 = 1_767_225_600_000;
    const MOVED: &str = "a provider credential must become the provider's default account";
    const OLD_REMOVED: &str = "a migrated provider must leave no old file or lock behind";
    const UNTOUCHED: &str = "files that are not provider credentials must stay where they are";
    const IDEMPOTENT: &str = "a second run must find nothing left to move";
    const NEW_KEPT: &str = "an existing default account must win over the leftover old file";
    const ONCE: &str = "the process-wide wrapper must migrate a root only once";

    fn state() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn legacy_path(dir: &StateDir, stem: &str, extension: &str) -> PathBuf {
        dir.persistent_path()
            .join(AUTH_DIR)
            .join(format!("{stem}.{extension}"))
    }

    fn write_legacy(dir: &StateDir, stem: &str, value: &serde_json::Value) -> PathBuf {
        let path = legacy_path(dir, stem, RECORD_EXTENSION);
        save_auth(&path, value).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(MODIFIED_MS))
            .unwrap();
        exclusive_state_lock(&legacy_path(dir, stem, LOCK_EXTENSION), AUTH_FILE_MODE).unwrap();
        path
    }

    fn default_id(provider: &str) -> AccountId {
        AccountId::new(provider, AccountName::new(DEFAULT_ACCOUNT).unwrap())
    }

    fn oauth_tokens() -> OAuthTokens {
        OAuthTokens {
            access: ACCESS.into(),
            refresh: REFRESH.into(),
            expires: EXPIRES,
            account_id: Some(OPENAI_ACCOUNT.into()),
        }
    }

    fn credentials(api_key: &str) -> ProviderCredentials {
        ProviderCredentials {
            api_key: api_key.into(),
            host: None,
        }
    }

    #[test_case(
        json!(oauth_tokens()),
        StoredSecret::OAuth(oauth_tokens())
        ; "oauth"
    )]
    #[test_case(
        json!(credentials(API_KEY)),
        StoredSecret::ApiKey(credentials(API_KEY))
        ; "api_key"
    )]
    fn provider_files_become_default_accounts(legacy: serde_json::Value, secret: StoredSecret) {
        let (_temp, dir) = state();
        write_legacy(&dir, OPENAI, &legacy);

        assert_eq!(migrate_legacy_auth(&dir).unwrap(), [OPENAI], "{MOVED}");

        assert_eq!(
            load_account(&dir, &default_id(OPENAI)).unwrap(),
            Some(StoredAccount {
                secret,
                identity: AccountIdentity::default(),
                created_at: MODIFIED_MS,
            }),
            "{MOVED}"
        );
        for extension in [RECORD_EXTENSION, LOCK_EXTENSION] {
            assert!(
                !legacy_path(&dir, OPENAI, extension).exists(),
                "{OLD_REMOVED}"
            );
        }
    }

    #[test]
    fn other_auth_files_are_left_alone() {
        let (_temp, dir) = state();
        let mcp = write_legacy(&dir, MCP_FILE, &json!(oauth_tokens()));
        let notes = write_legacy(&dir, NOT_A_CREDENTIAL, &json!({"remember": true}));

        assert!(migrate_legacy_auth(&dir).unwrap().is_empty(), "{UNTOUCHED}");

        assert!(mcp.is_file(), "{UNTOUCHED}");
        assert!(notes.is_file(), "{UNTOUCHED}");
        assert!(
            list_accounts(&dir, MCP_FILE).unwrap().is_empty(),
            "{UNTOUCHED}"
        );
    }

    #[test]
    fn a_second_run_moves_nothing() {
        let (_temp, dir) = state();
        write_legacy(&dir, ANTHROPIC, &json!(oauth_tokens()));
        migrate_legacy_auth(&dir).unwrap();
        let migrated = load_account(&dir, &default_id(ANTHROPIC)).unwrap();

        assert!(
            migrate_legacy_auth(&dir).unwrap().is_empty(),
            "{IDEMPOTENT}"
        );
        assert_eq!(
            load_account(&dir, &default_id(ANTHROPIC)).unwrap(),
            migrated,
            "{IDEMPOTENT}"
        );
    }

    #[test]
    fn a_crash_after_writing_keeps_the_new_account_and_removes_the_old_file() {
        let (_temp, dir) = state();
        let existing = StoredAccount {
            secret: StoredSecret::ApiKey(credentials(NEWER_KEY)),
            identity: AccountIdentity::default(),
            created_at: MODIFIED_MS + 1,
        };
        save_account(&dir, &default_id(ANTHROPIC), &existing).unwrap();
        let legacy = write_legacy(&dir, ANTHROPIC, &json!(credentials(API_KEY)));

        assert_eq!(migrate_legacy_auth(&dir).unwrap(), [ANTHROPIC]);

        assert_eq!(
            load_account(&dir, &default_id(ANTHROPIC)).unwrap(),
            Some(existing),
            "{NEW_KEPT}"
        );
        assert!(!legacy.exists(), "{OLD_REMOVED}");
    }

    #[test]
    fn the_wrapper_migrates_each_root_once() {
        let (_temp, dir) = state();
        write_legacy(&dir, ANTHROPIC, &json!(oauth_tokens()));
        ensure_legacy_auth_migrated(&dir);
        let late = write_legacy(&dir, OPENAI, &json!(credentials(API_KEY)));

        ensure_legacy_auth_migrated(&dir);

        assert!(
            load_account(&dir, &default_id(ANTHROPIC))
                .unwrap()
                .is_some(),
            "{MOVED}"
        );
        assert!(late.is_file(), "{ONCE}");
    }
}
