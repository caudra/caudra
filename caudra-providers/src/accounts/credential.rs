use std::fmt;

use caudra_storage::accounts::{AccountId, AccountIdentity, StoredAccount, StoredSecret};
use caudra_storage::auth::OAuthTokens;
use sha2::{Digest, Sha256};

use crate::AgentError;
use crate::model::Billing;
use crate::providers::copilot::auth::CopilotIdentity;

const FINGERPRINT_HEX_CHARS: usize = 12;
const MASK_EDGE_CHARS: usize = 4;
const MASKED_SHORT_KEY: &str = "****";

/// The secret one account signs requests with.
#[derive(Clone, PartialEq, Eq)]
pub enum AccountSecret {
    OAuth(OAuthTokens),
    ApiKey {
        key: String,
        host: Option<String>,
    },
    /// Local servers, gateways, and script or cloud-SDK providers that bring
    /// their own auth.
    Keyless,
}

impl From<StoredSecret> for AccountSecret {
    fn from(secret: StoredSecret) -> Self {
        match secret {
            StoredSecret::OAuth(tokens) => Self::OAuth(tokens),
            StoredSecret::ApiKey(credentials) => Self::ApiKey {
                key: credentials.api_key,
                host: credentials.host,
            },
        }
    }
}

/// Where the secret came from, which decides whether it can be refreshed,
/// renamed, or removed from Caudra.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CredentialSource {
    /// `auth/accounts/<provider>/<name>.json`.
    Stored,
    /// The named environment variable.
    Env(String),
    /// The provider-level `api_key` in `providers.toml`.
    Config,
}

/// Everything a provider instance needs to sign requests as one account. A
/// provider never reads storage or the environment for its credential itself.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountCredential {
    pub id: AccountId,
    pub secret: AccountSecret,
    pub source: CredentialSource,
    pub identity: AccountIdentity,
}

impl AccountCredential {
    pub fn stored(id: AccountId, account: StoredAccount) -> Self {
        Self {
            id,
            secret: account.secret.into(),
            source: CredentialSource::Stored,
            identity: account.identity,
        }
    }

    /// A Copilot GitHub token signs in to a subscription just as a login does.
    pub fn billing(&self) -> Billing {
        Billing::from_oauth(
            self.is_oauth() || CopilotIdentity::from_slug(&self.id.provider).is_some(),
        )
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self.secret, AccountSecret::OAuth(_))
    }

    pub fn oauth_tokens(&self) -> Option<&OAuthTokens> {
        match &self.secret {
            AccountSecret::OAuth(tokens) => Some(tokens),
            AccountSecret::ApiKey { .. } | AccountSecret::Keyless => None,
        }
    }

    pub fn optional_api_key(&self) -> Option<&str> {
        match &self.secret {
            AccountSecret::ApiKey { key, .. } => Some(key),
            AccountSecret::OAuth(_) | AccountSecret::Keyless => None,
        }
    }

    pub fn api_key(&self) -> Result<&str, AgentError> {
        self.optional_api_key().ok_or_else(|| AgentError::Config {
            message: format!("account {} has no API key", self.id),
        })
    }

    pub fn host(&self) -> Option<&str> {
        match &self.secret {
            AccountSecret::ApiKey { host, .. } => host.as_deref(),
            AccountSecret::OAuth(_) | AccountSecret::Keyless => None,
        }
    }

    /// Stable across token refreshes and different for a replaced API key,
    /// so a cached provider instance is rebuilt only when it must be.
    pub fn fingerprint(&self) -> String {
        match &self.secret {
            AccountSecret::OAuth(_) => "oauth".to_owned(),
            AccountSecret::Keyless => "keyless".to_owned(),
            AccountSecret::ApiKey { key, host } => {
                let mut hasher = Sha256::new();
                hasher.update(key.as_bytes());
                if let Some(host) = host {
                    hasher.update([0]);
                    hasher.update(host.as_bytes());
                }
                let digest = hasher.finalize();
                let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
                format!("key:{}", &hex[..FINGERPRINT_HEX_CHARS])
            }
        }
    }

    /// `sk-a…b2c3`, for listings.
    pub fn masked_key(&self) -> Option<String> {
        self.optional_api_key().map(|key| {
            let chars: Vec<char> = key.chars().collect();
            if chars.len() > MASK_EDGE_CHARS * 2 {
                let head: String = chars[..MASK_EDGE_CHARS].iter().collect();
                let tail: String = chars[chars.len() - MASK_EDGE_CHARS..].iter().collect();
                format!("{head}…{tail}")
            } else {
                MASKED_SHORT_KEY.to_owned()
            }
        })
    }
}

impl fmt::Debug for AccountCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountCredential")
            .field("id", &self.id)
            .field("secret", &self.fingerprint())
            .field("source", &self.source)
            .field("identity", &self.identity)
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use caudra_storage::accounts::{AccountId, AccountIdentity, AccountName};

    use super::{AccountCredential, AccountSecret, CredentialSource};

    pub(crate) fn api_key_account(provider: &str, name: &str, key: &str) -> AccountCredential {
        AccountCredential {
            id: AccountId::new(provider, AccountName::new(name).unwrap()),
            secret: AccountSecret::ApiKey {
                key: key.to_owned(),
                host: None,
            },
            source: CredentialSource::Stored,
            identity: AccountIdentity::default(),
        }
    }

    pub(crate) fn keyless_account(provider: &str) -> AccountCredential {
        AccountCredential {
            id: AccountId::new(provider, AccountName::new("default").unwrap()),
            secret: AccountSecret::Keyless,
            source: CredentialSource::Config,
            identity: AccountIdentity::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_storage::accounts::{AccountName, StoredAccount};
    use test_case::test_case;

    use super::test_support::api_key_account;
    use super::*;

    const SECRET: &str = "sk-ant-secret-value-1234";
    const ACCESS: &str = "access-token";
    const CREATED_AT: u64 = 1_767_225_600_000;

    fn oauth_account(provider: &str) -> AccountCredential {
        AccountCredential::stored(
            AccountId::new(provider, AccountName::new("personal").unwrap()),
            StoredAccount {
                secret: StoredSecret::OAuth(OAuthTokens {
                    access: ACCESS.into(),
                    refresh: ACCESS.into(),
                    expires: u64::MAX,
                    account_id: None,
                }),
                identity: AccountIdentity::default(),
                created_at: CREATED_AT,
            },
        )
    }

    #[test_case(oauth_account("anthropic"), Billing::Subscription ; "login")]
    #[test_case(api_key_account("anthropic", "team", SECRET), Billing::Api ; "api_key")]
    #[test_case(api_key_account("copilot", "team", SECRET), Billing::Subscription ; "copilot_token")]
    #[test_case(api_key_account("copilot-enterprise", "team", SECRET), Billing::Subscription ; "copilot_enterprise_token")]
    fn billing_follows_how_the_account_signs_in(account: AccountCredential, expected: Billing) {
        assert_eq!(account.billing(), expected);
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let account = api_key_account("anthropic", "team", SECRET);
        assert!(!format!("{account:?}").contains(SECRET));
    }

    #[test_case("sk-ant-secret-value-1234", "sk-a…1234" ; "long")]
    #[test_case("short", "****" ; "short")]
    fn keys_are_masked(key: &str, expected: &str) {
        let account = api_key_account("anthropic", "team", key);
        assert_eq!(account.masked_key().as_deref(), Some(expected));
    }

    #[test]
    fn fingerprints_follow_the_key() {
        let first = api_key_account("anthropic", "team", SECRET);
        let same = api_key_account("anthropic", "team", SECRET);
        let other = api_key_account("anthropic", "team", "sk-other");
        assert_eq!(first.fingerprint(), same.fingerprint());
        assert_ne!(first.fingerprint(), other.fingerprint());
    }
}
