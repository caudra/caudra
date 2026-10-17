//! Named provider accounts: their names, credential files, the selection the
//! user made per provider, and the quota health every Caudra process shares.

mod active;
mod credentials;
pub(crate) mod health;
mod migrate;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::auth::{MAX_NAME_BYTES, NameError, validate_name};

pub use active::{
    AccountSelection, persist_account_selection, read_account_selection, read_account_selections,
};
pub use credentials::{
    StoredAccount, StoredSecret, delete_account, list_account_providers, list_accounts,
    load_account, lock_account, rename_account, save_account, save_account_tokens_locked,
    try_lock_account, write_account_locked,
};
pub use health::{
    StoredAccountHealth, StoredQuotaWindow, delete_account_health, load_account_health,
    rename_account_health, save_account_health,
};
pub use migrate::{ensure_legacy_auth_migrated, migrate_legacy_auth};

/// The selection value that means "let the switching policy choose", so no
/// account may take it as a name.
pub const AUTO_SELECTION: &str = "auto";
/// What a credential migrated from the single-account layout is called.
pub const DEFAULT_ACCOUNT: &str = "default";
pub const MAX_PROVIDER_SLUG_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AccountNameError {
    #[error(transparent)]
    Invalid(#[from] NameError),
    #[error("`{AUTO_SELECTION}` is reserved for automatic switching")]
    Reserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderSlugError {
    #[error("provider slug must not be empty")]
    Empty,
    #[error("provider slug exceeds {MAX_PROVIDER_SLUG_BYTES} bytes")]
    TooLong,
    #[error("provider slug may contain only ASCII letters, digits, '-', '_', and inner '.'")]
    InvalidCharacter,
}

/// A provider slug is a directory name under `auth/accounts`, so it must not
/// traverse or hide.
pub fn validate_provider_slug(slug: &str) -> Result<(), ProviderSlugError> {
    let first = slug.bytes().next().ok_or(ProviderSlugError::Empty)?;
    if slug.len() > MAX_PROVIDER_SLUG_BYTES {
        return Err(ProviderSlugError::TooLong);
    }
    if first == b'.'
        || slug.contains("..")
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ProviderSlugError::InvalidCharacter);
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountName(String);

impl AccountName {
    pub fn new(name: impl Into<String>) -> Result<Self, AccountNameError> {
        let name = name.into();
        validate_name(&name)?;
        if name == AUTO_SELECTION {
            return Err(AccountNameError::Reserved);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccountName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("AccountName").field(&self.0).finish()
    }
}

impl fmt::Display for AccountName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AccountName {
    type Err = AccountNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl TryFrom<String> for AccountName {
    type Error = AccountNameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<AccountName> for String {
    fn from(name: AccountName) -> Self {
        name.0
    }
}

/// Who an account signs in as, read at login. It tells a second login as the
/// same person from a new account, names the account, and labels it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    /// The provider's stable id for the account or organization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl AccountIdentity {
    /// Whether both sides name the same account. Identities with nothing to
    /// compare never match.
    pub fn same_as(&self, other: &Self) -> bool {
        match (&self.subject, &other.subject) {
            (Some(left), Some(right)) => left == right,
            _ => self.email.is_some() && self.email == other.email,
        }
    }

    /// A valid account name drawn from the email's local part or the
    /// organization, for login to offer.
    pub fn suggested_name(&self) -> Option<AccountName> {
        let local_part = self
            .email
            .as_deref()
            .and_then(|email| email.split('@').next());
        [local_part, self.organization.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|raw| AccountName::new(slugify(raw)).ok())
    }
}

fn slugify(raw: &str) -> String {
    let mut slug = String::with_capacity(raw.len());
    for character in raw.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_end_matches('-');
    trimmed[..trimmed.len().min(MAX_NAME_BYTES)]
        .trim_end_matches('-')
        .to_owned()
}

/// One account of one provider, shown as `provider:name`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId {
    pub provider: String,
    pub name: AccountName,
}

impl AccountId {
    pub fn new(provider: impl Into<String>, name: AccountName) -> Self {
        Self {
            provider: provider.into(),
            name,
        }
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.provider, self.name)
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("personal" ; "plain")]
    #[test_case("env-2" ; "numbered")]
    #[test_case("work_team" ; "underscore")]
    fn account_names_accept_slugs(name: &str) {
        assert_eq!(AccountName::new(name).unwrap().as_str(), name);
    }

    #[test_case("", AccountNameError::Invalid(NameError::Empty) ; "empty")]
    #[test_case("auto", AccountNameError::Reserved ; "reserved")]
    #[test_case("Team", AccountNameError::Invalid(NameError::InvalidStart) ; "uppercase")]
    #[test_case("a/b", AccountNameError::Invalid(NameError::InvalidCharacter) ; "separator")]
    fn account_names_are_validated(name: &str, expected: AccountNameError) {
        assert_eq!(AccountName::new(name), Err(expected));
    }

    #[test_case("anthropic" ; "builtin")]
    #[test_case("ninfer-4090" ; "custom")]
    #[test_case("fireworks.ai" ; "dotted")]
    fn provider_slugs_accept_directory_names(slug: &str) {
        assert_eq!(validate_provider_slug(slug), Ok(()));
    }

    #[test_case("", ProviderSlugError::Empty ; "empty")]
    #[test_case("..", ProviderSlugError::InvalidCharacter ; "parent")]
    #[test_case(".hidden", ProviderSlugError::InvalidCharacter ; "hidden")]
    #[test_case("a/b", ProviderSlugError::InvalidCharacter ; "separator")]
    fn provider_slugs_are_validated(slug: &str, expected: ProviderSlugError) {
        assert_eq!(validate_provider_slug(slug), Err(expected));
    }

    #[test_case(Some("Jane.Doe@example.com"), None, Some("jane-doe") ; "email_local_part")]
    #[test_case(None, Some("Acme Corp"), Some("acme-corp") ; "organization")]
    #[test_case(Some("auto@example.com"), None, None ; "reserved_local_part")]
    #[test_case(None, None, None ; "nothing")]
    fn identities_suggest_names(
        email: Option<&str>,
        organization: Option<&str>,
        expected: Option<&str>,
    ) {
        let identity = AccountIdentity {
            email: email.map(str::to_owned),
            organization: organization.map(str::to_owned),
            ..AccountIdentity::default()
        };
        assert_eq!(
            identity.suggested_name().as_ref().map(AccountName::as_str),
            expected
        );
    }

    #[test_case(Some("u1"), None, Some("u1"), None, true ; "same_subject")]
    #[test_case(Some("u1"), Some("a@x"), Some("u2"), Some("a@x"), false ; "subject_wins")]
    #[test_case(None, Some("a@x"), None, Some("a@x"), true ; "same_email")]
    #[test_case(None, None, None, None, false ; "nothing_to_compare")]
    fn identities_compare_subject_then_email(
        left_subject: Option<&str>,
        left_email: Option<&str>,
        right_subject: Option<&str>,
        right_email: Option<&str>,
        expected: bool,
    ) {
        let identity = |subject: Option<&str>, email: Option<&str>| AccountIdentity {
            subject: subject.map(str::to_owned),
            email: email.map(str::to_owned),
            ..AccountIdentity::default()
        };
        assert_eq!(
            identity(left_subject, left_email).same_as(&identity(right_subject, right_email)),
            expected
        );
    }

    #[test]
    fn account_ids_display_provider_and_name() {
        let id = AccountId::new("anthropic", AccountName::new("team").unwrap());
        assert_eq!(id.to_string(), "anthropic:team");
    }
}
