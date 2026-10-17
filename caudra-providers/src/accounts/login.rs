//! What a sign-in brings back, and which stored account it lands in: the one
//! it names, the one it signs in as again, or a new one named after it.

use caudra_storage::accounts::{
    AccountId, AccountIdentity, AccountName, DEFAULT_ACCOUNT, StoredAccount, StoredSecret,
    list_accounts, load_account, save_account,
};
use caudra_storage::auth::now_millis;
use caudra_storage::{StateDir, StorageError};

use super::registry::invalidate;
use crate::AgentError;

const NUMBERED_ACCOUNT_PREFIX: &str = "account";
const FIRST_NUMBERED_ACCOUNT: usize = 2;

/// A credential a login produced, not yet stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginCredential {
    pub secret: StoredSecret,
    pub identity: AccountIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedLogin {
    pub id: AccountId,
    /// The login replaced the credential of an account that already existed.
    pub refreshed: bool,
}

/// Where a login without a name lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginTarget {
    /// The stored account it signs in as again.
    Existing(AccountName),
    /// A new account, under the name [`save_login`] would give it.
    New(AccountName),
}

/// Where [`save_login`] puts `login` when it is given no name, so a caller
/// can offer the name before it is used.
pub fn login_target(
    dir: &StateDir,
    provider: &str,
    login: &LoginCredential,
) -> Result<LoginTarget, AgentError> {
    let existing = list_accounts(dir, provider)?;
    Ok(match signed_in_as(&existing, login) {
        Some((name, _)) => LoginTarget::Existing(name.clone()),
        None => LoginTarget::New(free_name(&existing, &login.identity)?),
    })
}

/// Stores `login` as `name`, or else in the account it signs in as again, or
/// else in a new account named by [`suggest_account_name`].
pub fn save_login(
    dir: &StateDir,
    provider: &str,
    login: LoginCredential,
    name: Option<&AccountName>,
) -> Result<SavedLogin, AgentError> {
    let (name, previous) = match name {
        Some(name) => (
            name.clone(),
            load_account(dir, &AccountId::new(provider, name.clone()))?,
        ),
        None => {
            let existing = list_accounts(dir, provider)?;
            match signed_in_as(&existing, &login) {
                Some((name, account)) => (name.clone(), Some(account.clone())),
                None => (free_name(&existing, &login.identity)?, None),
            }
        }
    };
    let id = AccountId::new(provider, name);
    let refreshed = previous.is_some();
    let (created_at, identity) = match previous {
        Some(previous) if login.identity == AccountIdentity::default() => {
            (previous.created_at, previous.identity)
        }
        Some(previous) => (previous.created_at, login.identity),
        None => (now_millis(), login.identity),
    };
    save_account(
        dir,
        &id,
        &StoredAccount {
            secret: login.secret,
            identity,
            created_at,
        },
    )?;
    invalidate(Some(provider));
    Ok(SavedLogin { id, refreshed })
}

/// The name [`save_login`] gives a new account signing in as `identity`.
pub fn suggest_account_name(
    dir: &StateDir,
    provider: &str,
    identity: &AccountIdentity,
) -> Result<AccountName, AgentError> {
    free_name(&list_accounts(dir, provider)?, identity)
}

fn signed_in_as<'a>(
    existing: &'a [(AccountName, StoredAccount)],
    login: &LoginCredential,
) -> Option<&'a (AccountName, StoredAccount)> {
    existing
        .iter()
        .find(|(_, account)| signs_in_as(account, login))
}

fn signs_in_as(account: &StoredAccount, login: &LoginCredential) -> bool {
    match (&account.secret, &login.secret) {
        (StoredSecret::ApiKey(stored), StoredSecret::ApiKey(new))
            if stored.api_key == new.api_key =>
        {
            true
        }
        _ => account.identity.same_as(&login.identity),
    }
}

fn free_name(
    existing: &[(AccountName, StoredAccount)],
    identity: &AccountIdentity,
) -> Result<AccountName, AgentError> {
    let taken = |name: &AccountName| existing.iter().any(|(existing, _)| existing == name);
    if let Some(name) = identity.suggested_name().filter(|name| !taken(name)) {
        return Ok(name);
    }
    if existing.is_empty() {
        return account_name(DEFAULT_ACCOUNT.to_owned());
    }
    let mut number = FIRST_NUMBERED_ACCOUNT;
    loop {
        let name = account_name(format!("{NUMBERED_ACCOUNT_PREFIX}-{number}"))?;
        if !taken(&name) {
            return Ok(name);
        }
        number += 1;
    }
}

fn account_name(name: String) -> Result<AccountName, AgentError> {
    AccountName::new(name).map_err(|error| StorageError::from(error).into())
}

#[cfg(test)]
mod tests {
    use caudra_storage::accounts::save_account;
    use caudra_storage::auth::{OAuthTokens, ProviderCredentials};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const PROVIDER: &str = "anthropic";
    const EMAIL: &str = "jane.doe@example.com";
    const SUGGESTED: &str = "jane-doe";
    const OTHER_EMAIL: &str = "sam@example.com";
    const SUBJECT: &str = "user-1";
    const API_KEY: &str = "sk-ant-first";
    const OTHER_API_KEY: &str = "sk-ant-second";
    const ACCESS: &str = "access";
    const NEW_ACCESS: &str = "new-access";
    const CREATED_AT: u64 = 1_767_225_600_000;
    const WORK: &str = "work";
    const SAME_ACCOUNT: &str = "a second login as the same account must refresh it in place";
    const KEEPS_CREATION: &str = "a refreshed account keeps when it was first added";

    fn state() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn name(name: &str) -> AccountName {
        AccountName::new(name).unwrap()
    }

    fn identity(subject: Option<&str>, email: Option<&str>) -> AccountIdentity {
        AccountIdentity {
            subject: subject.map(str::to_owned),
            email: email.map(str::to_owned),
            ..AccountIdentity::default()
        }
    }

    fn oauth(access: &str, identity: AccountIdentity) -> LoginCredential {
        LoginCredential {
            secret: StoredSecret::OAuth(OAuthTokens {
                access: access.into(),
                refresh: access.into(),
                expires: u64::MAX,
                account_id: None,
            }),
            identity,
        }
    }

    fn api_key(key: &str) -> LoginCredential {
        LoginCredential {
            secret: StoredSecret::ApiKey(ProviderCredentials {
                api_key: key.into(),
                host: None,
            }),
            identity: AccountIdentity::default(),
        }
    }

    fn store(dir: &StateDir, account: &str, login: LoginCredential) {
        save_account(
            dir,
            &AccountId::new(PROVIDER, name(account)),
            &StoredAccount {
                secret: login.secret,
                identity: login.identity,
                created_at: CREATED_AT,
            },
        )
        .unwrap();
    }

    fn stored(dir: &StateDir, account: &str) -> StoredAccount {
        load_account(dir, &AccountId::new(PROVIDER, name(account)))
            .unwrap()
            .unwrap()
    }

    #[test_case(Some(EMAIL), &[], SUGGESTED ; "named_after_the_email")]
    #[test_case(None, &[], DEFAULT_ACCOUNT ; "first_without_identity_is_default")]
    #[test_case(None, &[WORK], "account-2" ; "next_without_identity_is_numbered")]
    #[test_case(Some(EMAIL), &[SUGGESTED], "account-2" ; "suggestion_taken_by_another")]
    #[test_case(None, &[WORK, "account-2"], "account-3" ; "skips_taken_numbers")]
    fn new_logins_get_a_free_name(email: Option<&str>, taken: &[&str], expected: &str) {
        let (_temp, dir) = state();
        for (index, account) in taken.iter().enumerate() {
            store(
                &dir,
                account,
                oauth(ACCESS, identity(Some(&index.to_string()), None)),
            );
        }
        let login = oauth(NEW_ACCESS, identity(Some(SUBJECT), email));

        assert_eq!(
            suggest_account_name(&dir, PROVIDER, &login.identity).unwrap(),
            name(expected)
        );
        let saved = save_login(&dir, PROVIDER, login, None).unwrap();

        assert_eq!(saved.id, AccountId::new(PROVIDER, name(expected)));
        assert!(!saved.refreshed);
    }

    #[test_case(identity(Some(SUBJECT), None), identity(Some(SUBJECT), Some(OTHER_EMAIL)) ; "same_subject")]
    #[test_case(identity(None, Some(EMAIL)), identity(None, Some(EMAIL)) ; "same_email")]
    fn logins_as_the_same_account_refresh_it(first: AccountIdentity, second: AccountIdentity) {
        let (_temp, dir) = state();
        store(&dir, WORK, oauth(ACCESS, first));

        let saved = save_login(&dir, PROVIDER, oauth(NEW_ACCESS, second.clone()), None).unwrap();

        assert_eq!(
            saved.id,
            AccountId::new(PROVIDER, name(WORK)),
            "{SAME_ACCOUNT}"
        );
        assert!(saved.refreshed, "{SAME_ACCOUNT}");
        let account = stored(&dir, WORK);
        assert_eq!(account.identity, second);
        assert_eq!(account.created_at, CREATED_AT, "{KEEPS_CREATION}");
        assert_eq!(account.secret, oauth(NEW_ACCESS, second).secret);
    }

    #[test_case(API_KEY, LoginTarget::Existing(name(WORK)) ; "same_key_targets_its_account")]
    #[test_case(OTHER_API_KEY, LoginTarget::New(name("account-2")) ; "other_key_targets_a_new_one")]
    fn a_login_target_is_where_save_login_puts_it(key: &str, expected: LoginTarget) {
        let (_temp, dir) = state();
        store(&dir, WORK, api_key(API_KEY));

        let target = login_target(&dir, PROVIDER, &api_key(key)).unwrap();
        let saved = save_login(&dir, PROVIDER, api_key(key), None).unwrap();

        assert_eq!(target, expected);
        let (LoginTarget::Existing(name) | LoginTarget::New(name)) = target;
        assert_eq!(saved.id.name, name);
    }

    #[test_case(API_KEY, true ; "same_key_refreshes")]
    #[test_case(OTHER_API_KEY, false ; "other_key_adds")]
    fn api_keys_match_by_value(key: &str, refreshed: bool) {
        let (_temp, dir) = state();
        store(&dir, WORK, api_key(API_KEY));

        let saved = save_login(&dir, PROVIDER, api_key(key), None).unwrap();

        assert_eq!(saved.refreshed, refreshed);
        assert_eq!(saved.id.name == name(WORK), refreshed);
    }

    #[test]
    fn a_named_login_replaces_that_account_and_keeps_its_identity_when_unknown() {
        let (_temp, dir) = state();
        let known = identity(Some(SUBJECT), Some(EMAIL));
        store(&dir, WORK, oauth(ACCESS, known.clone()));

        let saved = save_login(
            &dir,
            PROVIDER,
            oauth(NEW_ACCESS, AccountIdentity::default()),
            Some(&name(WORK)),
        )
        .unwrap();

        assert!(saved.refreshed);
        let account = stored(&dir, WORK);
        assert_eq!(account.identity, known);
        assert_eq!(account.created_at, CREATED_AT, "{KEEPS_CREATION}");
    }

    #[test]
    fn a_named_login_creates_a_missing_account() {
        let (_temp, dir) = state();

        let saved = save_login(&dir, PROVIDER, api_key(API_KEY), Some(&name(WORK))).unwrap();

        assert_eq!(saved.id, AccountId::new(PROVIDER, name(WORK)));
        assert!(!saved.refreshed);
        assert_eq!(stored(&dir, WORK).secret, api_key(API_KEY).secret);
    }
}
