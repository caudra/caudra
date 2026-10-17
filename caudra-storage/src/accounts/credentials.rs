//! One credential file per named account, `auth/accounts/<provider>/<name>.json`,
//! owner-only, beside the `<name>.lock` its writers serialize on.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{AccountId, AccountIdentity, AccountName, validate_provider_slug};
use crate::auth::{
    AUTH_DIR, AUTH_FILE_MODE, OAuthTokens, ProviderCredentials, delete_auth,
    read_private_credential, save_auth,
};
use crate::{
    StateDir, StorageError, durable_rename, exclusive_state_lock, sync_parent_dir,
    try_exclusive_state_lock,
};

const ACCOUNTS_DIR: &str = "accounts";
const RECORD_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 64 * 1024;
pub(super) const RECORD_EXTENSION: &str = "json";
pub(super) const LOCK_EXTENSION: &str = "lock";

/// What signs an account in. Tagged by `kind` so a record names its own
/// shape rather than leaving a reader to sniff fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StoredSecret {
    #[serde(rename = "oauth")]
    OAuth(OAuthTokens),
    #[serde(rename = "api_key")]
    ApiKey(ProviderCredentials),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    pub secret: StoredSecret,
    pub identity: AccountIdentity,
    /// Unix milliseconds of the first login, which orders the accounts no
    /// configuration places.
    pub created_at: u64,
}

#[derive(Serialize)]
struct RecordRef<'a> {
    version: u32,
    #[serde(flatten)]
    secret: &'a StoredSecret,
    identity: &'a AccountIdentity,
    created_at: u64,
}

#[derive(Deserialize)]
struct Record {
    version: u32,
    #[serde(flatten)]
    secret: StoredSecret,
    #[serde(default)]
    identity: AccountIdentity,
    created_at: u64,
}

fn accounts_root(dir: &StateDir) -> PathBuf {
    dir.persistent_path().join(AUTH_DIR).join(ACCOUNTS_DIR)
}

fn account_file(dir: &StateDir, id: &AccountId, extension: &str) -> Result<PathBuf, StorageError> {
    validate_provider_slug(&id.provider)?;
    Ok(accounts_root(dir)
        .join(&id.provider)
        .join(format!("{}.{extension}", id.name)))
}

fn record_exists(path: &Path) -> Result<bool, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn read_account(path: &Path) -> Result<Option<StoredAccount>, StorageError> {
    let Some(data) =
        read_private_credential(path, MAX_RECORD_BYTES, StorageError::InvalidAccountRecord)?
    else {
        return Ok(None);
    };
    let record: Record = serde_json::from_str(&data).map_err(|error| {
        StorageError::InvalidAccountRecord(format!("{}: {error}", path.display()))
    })?;
    if record.version != RECORD_VERSION {
        return Err(StorageError::InvalidAccountRecord(format!(
            "{}: record version {} is not {RECORD_VERSION}; a newer Caudra wrote it",
            path.display(),
            record.version
        )));
    }
    Ok(Some(StoredAccount {
        secret: record.secret,
        identity: record.identity,
        created_at: record.created_at,
    }))
}

pub fn load_account(dir: &StateDir, id: &AccountId) -> Result<Option<StoredAccount>, StorageError> {
    read_account(&account_file(dir, id, RECORD_EXTENSION)?)
}

pub(super) fn account_exists(dir: &StateDir, id: &AccountId) -> Result<bool, StorageError> {
    record_exists(&account_file(dir, id, RECORD_EXTENSION)?)
}

pub fn save_account(
    dir: &StateDir,
    id: &AccountId,
    account: &StoredAccount,
) -> Result<(), StorageError> {
    let _lock = lock_account(dir, id)?;
    write_account_locked(dir, id, account)
}

/// For a caller already holding [`lock_account`].
pub fn write_account_locked(
    dir: &StateDir,
    id: &AccountId,
    account: &StoredAccount,
) -> Result<(), StorageError> {
    save_auth(
        &account_file(dir, id, RECORD_EXTENSION)?,
        &RecordRef {
            version: RECORD_VERSION,
            secret: &account.secret,
            identity: &account.identity,
            created_at: account.created_at,
        },
    )
}

/// Replaces a refreshed OAuth account's tokens and keeps who it is and when
/// it was added. For a caller already holding [`lock_account`]. A refresh
/// that lost a race with logout must not bring the account back, so a missing
/// account is an error rather than a fresh record.
pub fn save_account_tokens_locked(
    dir: &StateDir,
    id: &AccountId,
    tokens: &OAuthTokens,
) -> Result<(), StorageError> {
    let mut account =
        load_account(dir, id)?.ok_or_else(|| StorageError::NotFound(format!("account {id}")))?;
    if !matches!(account.secret, StoredSecret::OAuth(_)) {
        return Err(StorageError::InvalidAccountRecord(format!(
            "{id} holds an API key, so it has no OAuth tokens to refresh"
        )));
    }
    account.secret = StoredSecret::OAuth(tokens.clone());
    write_account_locked(dir, id, &account)
}

pub fn lock_account(dir: &StateDir, id: &AccountId) -> Result<File, StorageError> {
    exclusive_state_lock(&account_file(dir, id, LOCK_EXTENSION)?, AUTH_FILE_MODE)
}

pub fn try_lock_account(dir: &StateDir, id: &AccountId) -> Result<Option<File>, StorageError> {
    try_exclusive_state_lock(&account_file(dir, id, LOCK_EXTENSION)?, AUTH_FILE_MODE)
}

pub fn delete_account(dir: &StateDir, id: &AccountId) -> Result<bool, StorageError> {
    let _lock = lock_account(dir, id)?;
    delete_auth(&account_file(dir, id, RECORD_EXTENSION)?)
}

/// Both locks are taken in name order, so two renames of the same pair in
/// opposite directions cannot deadlock.
pub fn rename_account(
    dir: &StateDir,
    provider: &str,
    from: &AccountName,
    to: &AccountName,
) -> Result<(), StorageError> {
    let source = AccountId::new(provider, from.clone());
    let target = AccountId::new(provider, to.clone());
    if from == to {
        return Err(StorageError::AccountExists(target.to_string()));
    }
    let (first, second) = if from < to {
        (&source, &target)
    } else {
        (&target, &source)
    };
    let _first = lock_account(dir, first)?;
    let _second = lock_account(dir, second)?;
    let destination = account_file(dir, &target, RECORD_EXTENSION)?;
    if record_exists(&destination)? {
        return Err(StorageError::AccountExists(target.to_string()));
    }
    let origin = account_file(dir, &source, RECORD_EXTENSION)?;
    if !record_exists(&origin)? {
        return Err(StorageError::NotFound(format!("account {source}")));
    }
    durable_rename(&origin, &destination)?;
    sync_parent_dir(&destination);
    Ok(())
}

/// Oldest first, then by name. An unreadable entry is skipped with a warning
/// so one damaged file does not hide every other account.
pub fn list_accounts(
    dir: &StateDir,
    provider: &str,
) -> Result<Vec<(AccountName, StoredAccount)>, StorageError> {
    validate_provider_slug(provider)?;
    let mut accounts = Vec::new();
    for path in record_paths(&accounts_root(dir).join(provider))? {
        match read_entry(&path) {
            Ok(Some(entry)) => accounts.push(entry),
            Ok(None) => {}
            Err(error) => {
                warn!(path = %path.display(), %error, "skipping unreadable account credential");
            }
        }
    }
    accounts.sort_by(|(left_name, left), (right_name, right)| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left_name.cmp(right_name))
    });
    Ok(accounts)
}

/// Providers with at least one stored account, sorted.
pub fn list_account_providers(dir: &StateDir) -> Result<Vec<String>, StorageError> {
    let entries = match fs::read_dir(accounts_root(dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut providers = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Ok(provider) = entry.file_name().into_string() else {
            continue;
        };
        if entry.file_type()?.is_dir()
            && validate_provider_slug(&provider).is_ok()
            && !record_paths(&entry.path())?.is_empty()
        {
            providers.push(provider);
        }
    }
    providers.sort();
    Ok(providers)
}

fn record_paths(directory: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension() == Some(OsStr::new(RECORD_EXTENSION)) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn read_entry(path: &Path) -> Result<Option<(AccountName, StoredAccount)>, StorageError> {
    let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
    let name = AccountName::new(stem)?;
    Ok(read_account(path)?.map(|account| (name, account)))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const PROVIDER: &str = "anthropic";
    const OTHER_PROVIDER: &str = "openai";
    const PERSONAL: &str = "personal";
    const WORK: &str = "work";
    const TEAM: &str = "team";
    const ACCESS: &str = "access-token";
    const REFRESHED_ACCESS: &str = "refreshed-access-token";
    const REFRESH: &str = "refresh-token";
    const EXPIRES: u64 = 9_999_999_999_999;
    const OPENAI_ACCOUNT: &str = "acct_123";
    const API_KEY: &str = "sk-ant-test-key";
    const HOST: &str = "https://gateway.example.com";
    const EMAIL: &str = "jane@example.com";
    const CREATED_AT: u64 = 1_767_225_600_000;
    #[cfg(unix)]
    const OWNER_ONLY: u32 = 0o600;
    #[cfg(unix)]
    const GROUP_READABLE: u32 = 0o640;
    #[cfg(unix)]
    const MODE_MASK: u32 = 0o777;
    const ROUND_TRIPS: &str = "a stored account must load back exactly as written";
    const KIND_TAGGED: &str = "a record must name its own kind";
    const REFUSED: &str = "an unsafe credential file must be refused, not read";
    const TARGET_KEPT: &str = "a refused rename must leave both accounts as they were";
    const LIST_ORDER: &str = "accounts list oldest first, then by name";
    const TOKENS_KEEP_IDENTITY: &str = "refreshed tokens must keep identity and created_at";

    fn state() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn id(provider: &str, name: &str) -> AccountId {
        AccountId::new(provider, AccountName::new(name).unwrap())
    }

    fn tokens(access: &str) -> OAuthTokens {
        OAuthTokens {
            access: access.into(),
            refresh: REFRESH.into(),
            expires: EXPIRES,
            account_id: Some(OPENAI_ACCOUNT.into()),
        }
    }

    fn oauth(created_at: u64) -> StoredAccount {
        StoredAccount {
            secret: StoredSecret::OAuth(tokens(ACCESS)),
            identity: AccountIdentity {
                email: Some(EMAIL.into()),
                ..AccountIdentity::default()
            },
            created_at,
        }
    }

    fn api_key(created_at: u64) -> StoredAccount {
        StoredAccount {
            secret: StoredSecret::ApiKey(ProviderCredentials {
                api_key: API_KEY.into(),
                host: Some(HOST.into()),
            }),
            identity: AccountIdentity::default(),
            created_at,
        }
    }

    fn record_path(dir: &StateDir, account: &AccountId) -> PathBuf {
        account_file(dir, account, RECORD_EXTENSION).unwrap()
    }

    #[test_case(oauth(CREATED_AT), "oauth" ; "oauth")]
    #[test_case(api_key(CREATED_AT), "api_key" ; "api_key")]
    fn accounts_round_trip_tagged_by_kind(account: StoredAccount, kind: &str) {
        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);

        save_account(&dir, &account_id, &account).unwrap();

        assert_eq!(
            load_account(&dir, &account_id).unwrap(),
            Some(account),
            "{ROUND_TRIPS}"
        );
        let path = record_path(&dir, &account_id);
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["kind"], kind, "{KIND_TAGGED}");
        assert_eq!(written["version"], RECORD_VERSION, "{KIND_TAGGED}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & MODE_MASK,
                OWNER_ONLY
            );
        }
    }

    #[test]
    fn missing_accounts_load_as_none() {
        let (_temp, dir) = state();
        assert_eq!(load_account(&dir, &id(PROVIDER, PERSONAL)).unwrap(), None);
    }

    #[test_case(".." ; "parent")]
    #[test_case("a/b" ; "separator")]
    fn provider_slugs_are_validated_before_any_path(provider: &str) {
        let (_temp, dir) = state();
        assert!(matches!(
            save_account(&dir, &id(provider, PERSONAL), &oauth(CREATED_AT)),
            Err(StorageError::InvalidProviderSlug(_))
        ));
        assert!(matches!(
            list_accounts(&dir, provider),
            Err(StorageError::InvalidProviderSlug(_))
        ));
    }

    #[test]
    fn unsupported_versions_are_refused() {
        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        save_account(&dir, &account_id, &api_key(CREATED_AT)).unwrap();
        let path = record_path(&dir, &account_id);
        let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        record["version"] = json!(RECORD_VERSION + 1);
        save_auth(&path, &record).unwrap();

        assert!(matches!(
            load_account(&dir, &account_id),
            Err(StorageError::InvalidAccountRecord(_))
        ));
    }

    #[test]
    fn oversized_records_are_refused() {
        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        let mut account = oauth(CREATED_AT);
        account.identity.organization = Some("x".repeat(MAX_RECORD_BYTES));

        save_account(&dir, &account_id, &account).unwrap();

        assert!(
            matches!(
                load_account(&dir, &account_id),
                Err(StorageError::InvalidAccountRecord(_))
            ),
            "{REFUSED}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_records_are_refused() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        save_account(&dir, &account_id, &oauth(CREATED_AT)).unwrap();
        fs::set_permissions(
            record_path(&dir, &account_id),
            fs::Permissions::from_mode(GROUP_READABLE),
        )
        .unwrap();

        assert!(
            matches!(
                load_account(&dir, &account_id),
                Err(StorageError::InvalidAccountRecord(_))
            ),
            "{REFUSED}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_records_are_refused() {
        use std::os::unix::fs::symlink;

        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        let target = id(PROVIDER, WORK);
        save_account(&dir, &target, &oauth(CREATED_AT)).unwrap();
        symlink(record_path(&dir, &target), record_path(&dir, &account_id)).unwrap();

        assert!(
            matches!(
                load_account(&dir, &account_id),
                Err(StorageError::InvalidAccountRecord(_))
            ),
            "{REFUSED}"
        );
    }

    #[test]
    fn refreshed_tokens_keep_identity_and_creation() {
        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        let account = oauth(CREATED_AT);
        save_account(&dir, &account_id, &account).unwrap();

        let _lock = lock_account(&dir, &account_id).unwrap();
        save_account_tokens_locked(&dir, &account_id, &tokens(REFRESHED_ACCESS)).unwrap();

        assert_eq!(
            load_account(&dir, &account_id).unwrap(),
            Some(StoredAccount {
                secret: StoredSecret::OAuth(tokens(REFRESHED_ACCESS)),
                ..account
            }),
            "{TOKENS_KEEP_IDENTITY}"
        );
    }

    #[test_case(None ; "missing")]
    #[test_case(Some(api_key(CREATED_AT)) ; "api_key")]
    fn refreshed_tokens_need_an_oauth_account(existing: Option<StoredAccount>) {
        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        if let Some(account) = &existing {
            save_account(&dir, &account_id, account).unwrap();
        }

        assert!(save_account_tokens_locked(&dir, &account_id, &tokens(ACCESS)).is_err());
        assert_eq!(load_account(&dir, &account_id).unwrap(), existing);
    }

    #[test]
    fn try_lock_reports_a_held_lock() {
        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        let held = lock_account(&dir, &account_id).unwrap();

        assert!(try_lock_account(&dir, &account_id).unwrap().is_none());
        drop(held);
        assert!(try_lock_account(&dir, &account_id).unwrap().is_some());
    }

    #[test]
    fn delete_reports_whether_an_account_was_removed() {
        let (_temp, dir) = state();
        let account_id = id(PROVIDER, PERSONAL);
        save_account(&dir, &account_id, &oauth(CREATED_AT)).unwrap();

        assert!(delete_account(&dir, &account_id).unwrap());
        assert_eq!(load_account(&dir, &account_id).unwrap(), None);
        assert!(!delete_account(&dir, &account_id).unwrap());
    }

    #[test]
    fn rename_moves_the_record() {
        let (_temp, dir) = state();
        let from = AccountName::new(PERSONAL).unwrap();
        let to = AccountName::new(TEAM).unwrap();
        let account = oauth(CREATED_AT);
        save_account(&dir, &id(PROVIDER, PERSONAL), &account).unwrap();

        rename_account(&dir, PROVIDER, &from, &to).unwrap();

        assert_eq!(load_account(&dir, &id(PROVIDER, PERSONAL)).unwrap(), None);
        assert_eq!(
            load_account(&dir, &id(PROVIDER, TEAM)).unwrap(),
            Some(account)
        );
    }

    #[test_case(PERSONAL, WORK ; "existing_target")]
    #[test_case(PERSONAL, PERSONAL ; "same_name")]
    fn rename_refuses_an_existing_target(from: &str, to: &str) {
        let (_temp, dir) = state();
        save_account(&dir, &id(PROVIDER, PERSONAL), &oauth(CREATED_AT)).unwrap();
        save_account(&dir, &id(PROVIDER, WORK), &api_key(CREATED_AT)).unwrap();

        let result = rename_account(
            &dir,
            PROVIDER,
            &AccountName::new(from).unwrap(),
            &AccountName::new(to).unwrap(),
        );

        assert!(
            matches!(&result, Err(StorageError::AccountExists(target)) if *target == id(PROVIDER, to).to_string()),
            "{result:?}"
        );
        assert_eq!(
            load_account(&dir, &id(PROVIDER, PERSONAL)).unwrap(),
            Some(oauth(CREATED_AT)),
            "{TARGET_KEPT}"
        );
        assert_eq!(
            load_account(&dir, &id(PROVIDER, WORK)).unwrap(),
            Some(api_key(CREATED_AT)),
            "{TARGET_KEPT}"
        );
    }

    #[test]
    fn rename_of_a_missing_account_is_not_found() {
        let (_temp, dir) = state();
        assert!(matches!(
            rename_account(
                &dir,
                PROVIDER,
                &AccountName::new(PERSONAL).unwrap(),
                &AccountName::new(TEAM).unwrap(),
            ),
            Err(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn accounts_list_oldest_first_then_by_name_skipping_unreadable() {
        let (_temp, dir) = state();
        save_account(&dir, &id(PROVIDER, WORK), &oauth(CREATED_AT)).unwrap();
        save_account(&dir, &id(PROVIDER, TEAM), &api_key(CREATED_AT + 1)).unwrap();
        save_account(&dir, &id(PROVIDER, PERSONAL), &oauth(CREATED_AT)).unwrap();
        let broken = id(PROVIDER, "broken");
        save_account(&dir, &broken, &oauth(CREATED_AT)).unwrap();
        save_auth(
            &record_path(&dir, &broken),
            &json!({"version": RECORD_VERSION}),
        )
        .unwrap();

        let names: Vec<String> = list_accounts(&dir, PROVIDER)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect();

        assert_eq!(names, [PERSONAL, WORK, TEAM], "{LIST_ORDER}");
        assert!(list_accounts(&dir, OTHER_PROVIDER).unwrap().is_empty());
    }

    #[test]
    fn providers_list_only_those_with_accounts() {
        let (_temp, dir) = state();
        save_account(&dir, &id(OTHER_PROVIDER, PERSONAL), &oauth(CREATED_AT)).unwrap();
        save_account(&dir, &id(PROVIDER, PERSONAL), &oauth(CREATED_AT)).unwrap();
        let removed = id("mistral", PERSONAL);
        save_account(&dir, &removed, &api_key(CREATED_AT)).unwrap();
        delete_account(&dir, &removed).unwrap();

        assert_eq!(
            list_account_providers(&dir).unwrap(),
            [PROVIDER, OTHER_PROVIDER]
        );
    }
}
