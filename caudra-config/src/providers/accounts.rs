//! `[[SLUG.accounts]]` and `[SLUG.switching]`: the accounts a provider signs
//! in with, in priority order, and when Auto moves a session between them.

use std::collections::HashSet;
use std::mem;

use caudra_storage::accounts::{AccountName, AccountNameError};
use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use super::{ProviderDef, ProvidersFile, ProvidersFileError, rendered};
use crate::toml_document::{detach_table, merge_item, merge_table, set_value, to_table};
use crate::{ConfigField, ConfigValue};

pub const DEFAULT_SWITCH_BELOW: u8 = 5;
pub const DEFAULT_PREFER_BELOW: u8 = 20;
const MAX_PERCENT: u8 = 100;
/// `[SLUG.switching]` within a provider, and the cross-provider
/// `[switching]` at the top of the file.
pub const SWITCHING_KEY: &str = "switching";
pub const MIN_PROVIDER_ORDER: usize = 2;
const ACCOUNTS_KEY: &str = "accounts";
const NAME_KEY: &str = "name";
const DUPLICATE_ACCOUNT_MESSAGE: &str = "account names must be unique per provider";
const EMPTY_API_KEY_ENV_MESSAGE: &str = "must name an environment variable";
const THRESHOLDS_MESSAGE: &str = "needs 0 <= switch_below < prefer_below <= 100";
const EMPTY_WINDOWS_MESSAGE: &str =
    "must list at least one window, or be left out to watch every window";
const UNKNOWN_WINDOW_MESSAGE: &str = "`switching.windows` has an unknown quota window";
const UNKNOWN_ACCOUNT_MESSAGE: &str = "has no account named";
const ACCOUNT_EXISTS_MESSAGE: &str = "already has an account named";
const SHORT_PROVIDER_ORDER_MESSAGE: &str =
    "must list at least two providers, or be left out to keep conversations on their provider";
const DUPLICATE_PROVIDER_MESSAGE: &str = "lists a provider more than once";
const BLANK_PROVIDER_MESSAGE: &str = "has an empty provider slug";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AccountsConfigError {
    #[error("{provider}.accounts: `{name}` is not a valid account name: {source}")]
    InvalidName {
        provider: String,
        name: String,
        #[source]
        source: AccountNameError,
    },
    #[error("{provider}.accounts lists `{name}` more than once, and {DUPLICATE_ACCOUNT_MESSAGE}")]
    DuplicateName { provider: String, name: String },
    #[error("{provider}.accounts: the api_key_env of `{name}` {EMPTY_API_KEY_ENV_MESSAGE}")]
    EmptyApiKeyEnv { provider: String, name: String },
    #[error(
        "{provider}.switching {THRESHOLDS_MESSAGE}, but switch_below is {switch_below} and prefer_below is {prefer_below}"
    )]
    Thresholds {
        provider: String,
        switch_below: u8,
        prefer_below: u8,
    },
    #[error("{provider}.switching.windows {EMPTY_WINDOWS_MESSAGE}")]
    EmptyWindows { provider: String },
    #[error("{provider}.accounts {UNKNOWN_ACCOUNT_MESSAGE} `{name}`")]
    UnknownAccount { provider: String, name: String },
    #[error("{provider}.accounts {ACCOUNT_EXISTS_MESSAGE} `{name}`")]
    AccountExists { provider: String, name: String },
    #[error("{SWITCHING_KEY}.providers {SHORT_PROVIDER_ORDER_MESSAGE}, but lists {count}")]
    ShortProviderOrder { count: usize },
    #[error("{SWITCHING_KEY}.providers {DUPLICATE_PROVIDER_MESSAGE}: `{provider}`")]
    DuplicateProvider { provider: String },
    #[error("{SWITCHING_KEY}.providers {BLANK_PROVIDER_MESSAGE}")]
    BlankProvider,
}

/// One `[[SLUG.accounts]]` entry. The list order is the priority order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountDef {
    pub name: String,
    /// Where an API-key account reads its key, in place of the credential
    /// `caudra auth login` saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// A metered account Auto may move a subscription session onto, once
    /// every subscription is spent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
}

impl AccountDef {
    pub(crate) const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: NAME_KEY,
            ty: "string",
            default: ConfigValue::Required("\"personal\""),
            min: None,
            max: None,
            env: None,
            description: "The account name, which `caudra auth login --name` saves a credential under and `caudra auth use` selects. It takes lowercase letters, digits, `-`, and `_`, and `auto` is reserved",
        },
        ConfigField {
            name: "api_key_env",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The environment variable that holds this account's API key. Leave it out to use the credential `caudra auth login` saved",
        },
        ConfigField {
            name: "fallback",
            ty: "bool",
            default: ConfigValue::Bool(false),
            min: None,
            max: None,
            env: None,
            description: "Let Auto move a subscription session onto this pay-per-token account, which it does only when every subscription is spent. A subscription account ignores it",
        },
    ];

    fn validate(&self, provider: &str) -> Result<(), AccountsConfigError> {
        AccountName::new(self.name.as_str()).map_err(|source| {
            AccountsConfigError::InvalidName {
                provider: provider.into(),
                name: self.name.clone(),
                source,
            }
        })?;
        if self
            .api_key_env
            .as_deref()
            .is_some_and(|env| env.trim().is_empty())
        {
            return Err(AccountsConfigError::EmptyApiKeyEnv {
                provider: provider.into(),
                name: self.name.clone(),
            });
        }
        Ok(())
    }
}

/// How Auto ranks the accounts it may move a session to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchStrategy {
    /// The first usable account in priority order.
    #[default]
    Ordered,
    /// The account furthest ahead of its quota schedule.
    Paced,
}

impl SwitchStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ordered => "ordered",
            Self::Paced => "paced",
        }
    }
}

/// A quota window `switching.windows` can watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "&'static str")]
pub enum QuotaWindowName {
    FiveHour,
    Weekly,
    Monthly,
}

impl QuotaWindowName {
    pub const ALL: [Self; 3] = [Self::FiveHour, Self::Weekly, Self::Monthly];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
}

#[derive(Debug, Error)]
#[error("{UNKNOWN_WINDOW_MESSAGE} `{0}`, expected `5h`, `weekly`, or `monthly`")]
pub struct UnknownQuotaWindow(String);

impl TryFrom<String> for QuotaWindowName {
    type Error = UnknownQuotaWindow;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|window| window.as_str() == name)
            .ok_or(UnknownQuotaWindow(name))
    }
}

impl From<QuotaWindowName> for &'static str {
    fn from(window: QuotaWindowName) -> Self {
        window.as_str()
    }
}

/// `[SLUG.switching]`. Each threshold is the quota left in a watched window,
/// in percent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwitchingDef {
    #[serde(skip_serializing_if = "is_default_strategy")]
    pub strategy: SwitchStrategy,
    /// Below this, a session moves at its next request, even mid-cache.
    #[serde(skip_serializing_if = "is_default_switch_below")]
    pub switch_below: u8,
    /// Below this, a session moves only once its prompt cache is cold.
    #[serde(skip_serializing_if = "is_default_prefer_below")]
    pub prefer_below: u8,
    /// `None` watches every window the provider reports for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<QuotaWindowName>>,
}

impl Default for SwitchingDef {
    fn default() -> Self {
        Self {
            strategy: SwitchStrategy::default(),
            switch_below: DEFAULT_SWITCH_BELOW,
            prefer_below: DEFAULT_PREFER_BELOW,
            windows: None,
        }
    }
}

impl SwitchingDef {
    pub(crate) const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "strategy",
            ty: "string",
            default: ConfigValue::Str(SwitchStrategy::Ordered.as_str()),
            min: None,
            max: None,
            env: None,
            description: "How Auto picks an account. `ordered` takes the first usable account in `accounts` order and returns to it after its quota resets. `paced` takes the account furthest ahead of its quota schedule, so capacity that is about to reset gets used first",
        },
        ConfigField {
            name: "switch_below",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_SWITCH_BELOW as u64),
            min: Some(0),
            max: Some(MAX_PERCENT as u64 - 1),
            env: None,
            description: "The quota left in a watched window, in percent, below which a session moves to another account at its next request, even when that loses a warm prompt cache. It has to be lower than `prefer_below`",
        },
        ConfigField {
            name: "prefer_below",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_PREFER_BELOW as u64),
            min: Some(1),
            max: Some(MAX_PERCENT as u64),
            env: None,
            description: "The quota left in a watched window, in percent, below which a session moves to another account once its prompt cache is cold anyway, such as in a new session or after compaction",
        },
        ConfigField {
            name: "windows",
            ty: "string[]",
            default: ConfigValue::Varies("every window the provider reports for the model"),
            min: None,
            max: None,
            env: None,
            description: "The quota windows the thresholds watch: `5h`, `weekly`, and `monthly`",
        },
    ];

    fn validate(&self, provider: &str) -> Result<(), AccountsConfigError> {
        if self.switch_below >= self.prefer_below || self.prefer_below > MAX_PERCENT {
            return Err(AccountsConfigError::Thresholds {
                provider: provider.into(),
                switch_below: self.switch_below,
                prefer_below: self.prefer_below,
            });
        }
        if self.windows.as_ref().is_some_and(Vec::is_empty) {
            return Err(AccountsConfigError::EmptyWindows {
                provider: provider.into(),
            });
        }
        Ok(())
    }
}

/// The top-level `[switching]`: the providers Auto may move a conversation
/// across once every subscription of its own provider runs low, in the order
/// it tries them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalSwitchingDef {
    pub providers: Vec<String>,
}

impl GlobalSwitchingDef {
    pub(crate) const FIELDS: &[ConfigField] = &[ConfigField {
        name: "providers",
        ty: "string[]",
        default: ConfigValue::Required("[\"anthropic\", \"openai\"]"),
        min: None,
        max: None,
        env: None,
        description: "The provider slugs Auto may move a conversation across, in the order it tries them, when every subscription of the conversation's provider is spent or running low. The conversation moves to a subscription account of the next provider with quota left, on a model of the same size, and comes back once its own provider has quota again. Leave the table out to keep every conversation on its provider",
    }];

    fn validate(&self) -> Result<(), AccountsConfigError> {
        if self.providers.len() < MIN_PROVIDER_ORDER {
            return Err(AccountsConfigError::ShortProviderOrder {
                count: self.providers.len(),
            });
        }
        let mut seen = HashSet::new();
        for provider in &self.providers {
            if provider.trim().is_empty() {
                return Err(AccountsConfigError::BlankProvider);
            }
            if !seen.insert(provider.as_str()) {
                return Err(AccountsConfigError::DuplicateProvider {
                    provider: provider.clone(),
                });
            }
        }
        Ok(())
    }
}

pub(super) fn deserialize_global_switching<'de, D>(
    deserializer: D,
) -> Result<Option<GlobalSwitchingDef>, D::Error>
where
    D: Deserializer<'de>,
{
    let def = GlobalSwitchingDef::deserialize(deserializer)?;
    def.validate().map_err(D::Error::custom)?;
    Ok(Some(def))
}

fn is_default_strategy(strategy: &SwitchStrategy) -> bool {
    *strategy == SwitchStrategy::default()
}

fn is_default_switch_below(percent: &u8) -> bool {
    *percent == DEFAULT_SWITCH_BELOW
}

fn is_default_prefer_below(percent: &u8) -> bool {
    *percent == DEFAULT_PREFER_BELOW
}

/// What serde cannot check on its own: valid names that are unique within
/// the provider, and thresholds in order.
pub(super) fn validate(provider: &str, def: &ProviderDef) -> Result<(), AccountsConfigError> {
    let mut names = HashSet::new();
    for account in &def.accounts {
        account.validate(provider)?;
        if !names.insert(account.name.as_str()) {
            return Err(AccountsConfigError::DuplicateName {
                provider: provider.into(),
                name: account.name.clone(),
            });
        }
    }
    def.switching
        .as_ref()
        .map_or(Ok(()), |switching| switching.validate(provider))
}

impl ProvidersFile {
    /// Replaces the entry with the same name where it stands, or appends it
    /// at the lowest priority.
    pub fn upsert_account(
        &mut self,
        provider: &str,
        account: &AccountDef,
    ) -> Result<(), ProvidersFileError> {
        account.validate(provider)?;
        let desired = to_table(account)?;
        let index = self.account_index(provider, &account.name);
        let previous = index
            .and_then(|index| self.configured_accounts(provider).get(index))
            .map(to_table)
            .transpose()?;
        let tables = account_tables(&mut self.document, provider)?;
        match (index.and_then(|index| tables.get_mut(index)), previous) {
            (Some(current), Some(previous)) => merge_table(current, &previous, &desired),
            _ => {
                let mut desired = desired;
                detach_table(&mut desired);
                tables.push(desired);
            }
        }
        let accounts = &mut self
            .config
            .providers
            .entry(provider.to_owned())
            .or_default()
            .accounts;
        match index {
            Some(index) => accounts[index] = account.clone(),
            None => accounts.push(account.clone()),
        }
        Ok(())
    }

    pub fn remove_account(&mut self, provider: &str, name: &str) -> bool {
        let Some(index) = self.account_index(provider, name) else {
            return false;
        };
        if let Ok(tables) = account_tables(&mut self.document, provider)
            && index < tables.len()
        {
            tables.remove(index);
        }
        if let Some(def) = self.config.providers.get_mut(provider) {
            def.accounts.remove(index);
        }
        self.prune(provider);
        true
    }

    pub fn rename_account(
        &mut self,
        provider: &str,
        from: &str,
        to: &str,
    ) -> Result<(), ProvidersFileError> {
        let index = self.account_index(provider, from).ok_or_else(|| {
            AccountsConfigError::UnknownAccount {
                provider: provider.into(),
                name: from.into(),
            }
        })?;
        if from == to {
            return Ok(());
        }
        AccountName::new(to).map_err(|source| AccountsConfigError::InvalidName {
            provider: provider.into(),
            name: to.into(),
            source,
        })?;
        if self.account_index(provider, to).is_some() {
            return Err(AccountsConfigError::AccountExists {
                provider: provider.into(),
                name: to.into(),
            }
            .into());
        }
        if let Some(table) = account_tables(&mut self.document, provider)?.get_mut(index) {
            set_value(table, NAME_KEY, Value::from(to));
        }
        if let Some(account) = self
            .config
            .providers
            .get_mut(provider)
            .and_then(|def| def.accounts.get_mut(index))
        {
            account.name = to.into();
        }
        Ok(())
    }

    /// Moves `name` by `offset` places in the priority order, towards the
    /// front when negative, and stops at either end. False when it stayed.
    pub fn move_account(&mut self, provider: &str, name: &str, offset: isize) -> bool {
        let Some(from) = self.account_index(provider, name) else {
            return false;
        };
        let Some(accounts) = self
            .config
            .providers
            .get_mut(provider)
            .map(|def| &mut def.accounts)
        else {
            return false;
        };
        let to = from
            .saturating_add_signed(offset)
            .min(accounts.len().saturating_sub(1));
        if to == from {
            return false;
        }
        let account = accounts.remove(from);
        accounts.insert(to, account);
        if let Ok(tables) = account_tables(&mut self.document, provider) {
            reorder(tables, from, to);
        }
        true
    }

    /// Writes only the keys that differ from the defaults, and drops the
    /// table when none do.
    pub fn set_switching(
        &mut self,
        provider: &str,
        switching: &SwitchingDef,
    ) -> Result<(), ProvidersFileError> {
        switching.validate(provider)?;
        if *switching == SwitchingDef::default() {
            if let Some(table) = self
                .document
                .get_mut(provider)
                .and_then(Item::as_table_like_mut)
            {
                table.remove(SWITCHING_KEY);
            }
            if let Some(def) = self.config.providers.get_mut(provider) {
                def.switching = None;
            }
            self.prune(provider);
            return Ok(());
        }
        let previous = rendered(
            self.config
                .get(provider)
                .and_then(|def| def.switching.as_ref()),
        )?;
        let desired = Item::Table(to_table(switching)?);
        merge_item(
            provider_table(&mut self.document, provider)?,
            SWITCHING_KEY,
            previous.as_ref(),
            &desired,
        );
        self.config
            .providers
            .entry(provider.to_owned())
            .or_default()
            .switching = Some(switching.clone());
        Ok(())
    }

    /// The cross-provider order. An empty one drops `[switching]`, which
    /// keeps every conversation on its provider.
    pub fn set_provider_order(&mut self, providers: &[String]) -> Result<(), ProvidersFileError> {
        if providers.is_empty() {
            self.document.remove(SWITCHING_KEY);
            self.config.switching = None;
            return Ok(());
        }
        let switching = GlobalSwitchingDef {
            providers: providers.to_vec(),
        };
        switching.validate()?;
        let previous = rendered(self.config.switching.as_ref())?;
        let desired = Item::Table(to_table(&switching)?);
        merge_item(
            self.document.as_table_mut(),
            SWITCHING_KEY,
            previous.as_ref(),
            &desired,
        );
        self.config.switching = Some(switching);
        Ok(())
    }

    fn configured_accounts(&self, provider: &str) -> &[AccountDef] {
        self.config
            .get(provider)
            .map_or(&[], |def| def.accounts.as_slice())
    }

    fn account_index(&self, provider: &str, name: &str) -> Option<usize> {
        self.configured_accounts(provider)
            .iter()
            .position(|account| account.name == name)
    }

    /// Drops an `accounts` list an edit emptied, then a provider table that
    /// only existed to hold what the edit removed.
    fn prune(&mut self, provider: &str) {
        let Some(table) = self.document.get_mut(provider).and_then(Item::as_table_mut) else {
            return;
        };
        if table
            .get(ACCOUNTS_KEY)
            .and_then(Item::as_array_of_tables)
            .is_some_and(ArrayOfTables::is_empty)
        {
            table.remove(ACCOUNTS_KEY);
        }
        if table.is_empty() && table.is_implicit() {
            self.document.remove(provider);
            self.config.providers.remove(provider);
        }
    }
}

/// `[provider]`, created without a header of its own when the file lacks it.
/// An inline table becomes a standard one, since it cannot hold
/// `[[provider.accounts]]`.
fn provider_table<'a>(
    document: &'a mut DocumentMut,
    provider: &str,
) -> Result<&'a mut Table, toml::ser::Error> {
    let item = document.entry(provider).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    *item = mem::take(item)
        .into_table()
        .map_or_else(|kept| kept, Item::Table);
    item.as_table_mut()
        .ok_or_else(|| toml::ser::Error::custom(format!("`{provider}` must be a table")))
}

/// `[[provider.accounts]]`, from an inline array when the file wrote one.
fn account_tables<'a>(
    document: &'a mut DocumentMut,
    provider: &str,
) -> Result<&'a mut ArrayOfTables, toml::ser::Error> {
    let item = provider_table(document, provider)?
        .entry(ACCOUNTS_KEY)
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    *item = match mem::take(item).into_array_of_tables() {
        Ok(tables) => Item::ArrayOfTables(tables),
        Err(Item::Value(Value::Array(array))) if array.is_empty() => {
            Item::ArrayOfTables(ArrayOfTables::new())
        }
        Err(kept) => kept,
    };
    item.as_array_of_tables_mut().ok_or_else(|| {
        toml::ser::Error::custom(format!("`{provider}.{ACCOUNTS_KEY}` must be tables"))
    })
}

/// A table renders at its own document position, so the moved table takes
/// over the positions of the ones it passes, and every slot stays put.
fn reorder(tables: &mut ArrayOfTables, from: usize, to: usize) {
    if from.max(to) >= tables.len() {
        return;
    }
    let positions: Vec<Option<isize>> = tables.iter().map(Table::position).collect();
    let table = tables.remove(from);
    tables.insert(to, table);
    for (table, position) in tables.iter_mut().zip(positions) {
        table.set_position(position);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use caudra_storage::accounts::AUTO_SELECTION;

    use super::*;
    use crate::providers::tests::parse_config;
    use crate::providers::{PROVIDERS_FILE, ProvidersConfig, resolve_base_url};

    const PROVIDER: &str = "anthropic";
    const UNLISTED: &str = "xai";
    const OTHER_PROVIDER: &str = "openai";
    const UNKNOWN_STRATEGY_FIELD_MESSAGE: &str = "unknown field `strategy`";
    const ORDER_COMMENT: &str = "# order note";
    const PERSONAL: &str = "personal";
    const TEAM: &str = "team";
    const API: &str = "api";
    const ADDED: &str = "added";
    const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
    const RESERVED_NAME_MESSAGE: &str = "is reserved for automatic switching";
    const INVALID_START_MESSAGE: &str = "must start with a lowercase ASCII letter or digit";
    const UNKNOWN_FIELD_MESSAGE: &str = "unknown field `api_key`";
    const FILE_COMMENT: &str = "# providers I use";
    const PROVIDER_COMMENT: &str = "# my anthropic setup";
    const PROXY_COMMENT: &str = "# proxy note";
    const SWITCHING_COMMENT: &str = "# switching note";
    const PACE_COMMENT: &str = "# pace note";
    const PERSONAL_COMMENT: &str = "# personal note";
    const NAME_COMMENT: &str = "# name note";
    const TEAM_COMMENT: &str = "# team note";
    const API_COMMENT: &str = "# api note";
    const OTHER_TABLE: &str = "[openai]\nbase_url = \"https://openai.example/v1\"\n";
    const EDITED_FILE: &str = r#"# providers I use
version = 1

# my anthropic setup
[anthropic]
base_url = "https://proxy.example/v1" # proxy note

# switching note
[anthropic.switching]
strategy = "paced" # pace note

# personal note
[[anthropic.accounts]]
name = "personal" # name note

# team note
[[anthropic.accounts]]
name = "team"

# api note
[[anthropic.accounts]]
name = "api"
api_key_env = "ANTHROPIC_API_KEY"
fallback = true

[openai]
base_url = "https://openai.example/v1"
"#;
    const KEPT: [&str; 9] = [
        FILE_COMMENT,
        PROVIDER_COMMENT,
        PROXY_COMMENT,
        SWITCHING_COMMENT,
        PACE_COMMENT,
        PERSONAL_COMMENT,
        TEAM_COMMENT,
        API_COMMENT,
        OTHER_TABLE,
    ];

    fn edited() -> ProvidersFile {
        ProvidersFile::parse(EDITED_FILE).unwrap()
    }

    fn assert_survives(rendered: &str, kept: &[&str]) {
        for kept in kept {
            assert!(rendered.contains(kept), "{kept} lost:\n{rendered}");
        }
    }

    fn reparsed(file: &ProvidersFile) -> ProvidersFile {
        ProvidersFile::parse(&file.render()).unwrap()
    }

    fn names(file: &ProvidersFile) -> Vec<String> {
        file.configured_accounts(PROVIDER)
            .iter()
            .map(|account| account.name.clone())
            .collect()
    }

    fn account(name: &str) -> AccountDef {
        AccountDef {
            name: name.into(),
            api_key_env: None,
            fallback: false,
        }
    }

    fn parse_error(text: &str) -> String {
        parse_config(text).unwrap_err().to_string()
    }

    #[test]
    fn accounts_and_switching_parse_in_file_order() {
        let file = edited();
        let def = file.get(PROVIDER).unwrap();

        assert_eq!(names(&file), [PERSONAL, TEAM, API]);
        assert_eq!(
            def.accounts[2],
            AccountDef {
                name: API.into(),
                api_key_env: Some(API_KEY_ENV.into()),
                fallback: true,
            }
        );
        assert_eq!(
            def.switching,
            Some(SwitchingDef {
                strategy: SwitchStrategy::Paced,
                ..SwitchingDef::default()
            })
        );
    }

    #[test]
    fn an_empty_switching_table_takes_the_defaults() {
        let config: ProvidersConfig = parse_config("[anthropic.switching]\n").unwrap();
        let switching = config.get(PROVIDER).unwrap().switching.clone().unwrap();

        assert_eq!(switching.switch_below, DEFAULT_SWITCH_BELOW);
        assert_eq!(switching.prefer_below, DEFAULT_PREFER_BELOW);
        assert_eq!(switching.strategy, SwitchStrategy::Ordered);
        assert_eq!(switching.windows, None);
    }

    #[test_case("[[anthropic.accounts]]\nname = \"personal\"\n", true ; "accounts_only")]
    #[test_case("[anthropic.switching]\nstrategy = \"paced\"\n", true ; "switching_only")]
    #[test_case("[anthropic]\n", true ; "empty_table")]
    #[test_case("[anthropic]\nbase_url = \"https://proxy.example/v1\"\n[[anthropic.accounts]]\nname = \"personal\"\n", false ; "with_a_base_url")]
    #[test_case("[anthropic]\napi_key = \"sk-test\"\n", false ; "with_a_provider_key")]
    fn only_accounts_and_switching_leave_the_provider_alone(text: &str, accounts_only: bool) {
        let config: ProvidersConfig = parse_config(text).unwrap();

        assert_eq!(
            config.get(PROVIDER).unwrap().is_accounts_only(),
            accounts_only
        );
        assert_eq!(config.defines_provider(PROVIDER), !accounts_only);
    }

    #[test]
    fn an_accounts_only_table_keeps_the_built_in_base_url() {
        let slug = "caudra-test-accounts-only-slug";
        let config: ProvidersConfig =
            parse_config(&format!("[[{slug}.accounts]]\nname = \"personal\"\n")).unwrap();

        assert_eq!(
            resolve_base_url(slug, config.get(slug)),
            resolve_base_url(slug, None)
        );
    }

    #[test_case("[[anthropic.accounts]]\nname = \"Personal\"\n", INVALID_START_MESSAGE ; "invalid_name")]
    #[test_case("[[anthropic.accounts]]\nname = \"auto\"\n", RESERVED_NAME_MESSAGE ; "reserved_name")]
    #[test_case("[[anthropic.accounts]]\nname = \"team\"\n[[anthropic.accounts]]\nname = \"team\"\n", DUPLICATE_ACCOUNT_MESSAGE ; "duplicate_name")]
    #[test_case("[[anthropic.accounts]]\nname = \"api\"\napi_key_env = \" \"\n", EMPTY_API_KEY_ENV_MESSAGE ; "blank_api_key_env")]
    #[test_case("[anthropic.switching]\nswitch_below = 20\n", THRESHOLDS_MESSAGE ; "switch_not_below_prefer")]
    #[test_case("[anthropic.switching]\nprefer_below = 101\n", THRESHOLDS_MESSAGE ; "prefer_above_100")]
    #[test_case("[anthropic.switching]\nwindows = []\n", EMPTY_WINDOWS_MESSAGE ; "no_windows")]
    #[test_case("[anthropic.switching]\nwindows = [\"5h\", \"daily\"]\n", UNKNOWN_WINDOW_MESSAGE ; "unknown_window")]
    #[test_case("[[anthropic.accounts]]\nname = \"api\"\napi_key = \"sk\"\n", UNKNOWN_FIELD_MESSAGE ; "unknown_account_field")]
    fn invalid_accounts_config_names_the_provider(text: &str, expected: &str) {
        let error = parse_error(text);

        assert!(error.contains(expected), "{error}");
        assert!(error.contains(PROVIDER), "{error}");
    }

    #[test]
    fn loading_a_file_with_invalid_accounts_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROVIDERS_FILE);
        fs::write(&path, "[[anthropic.accounts]]\nname = \"auto\"\n").unwrap();

        let error = ProvidersConfig::parse_file(&path).unwrap_err();

        assert!(error.contains(RESERVED_NAME_MESSAGE), "{error}");
        assert!(ProvidersFile::parse(&fs::read_to_string(&path).unwrap()).is_err());
    }

    #[test]
    fn upserting_a_new_account_appends_it_last() {
        let mut file = edited();

        file.upsert_account(PROVIDER, &account(ADDED)).unwrap();
        let rendered = file.render();

        assert_survives(&rendered, &KEPT);
        assert!(
            rendered.find(ADDED) < rendered.find(OTHER_TABLE),
            "{rendered}"
        );
        assert_eq!(names(&reparsed(&file)), [PERSONAL, TEAM, API, ADDED]);
    }

    #[test]
    fn upserting_an_existing_account_replaces_it_in_place() {
        let mut file = edited();
        let changed = AccountDef {
            api_key_env: Some(API_KEY_ENV.into()),
            ..account(PERSONAL)
        };

        file.upsert_account(PROVIDER, &changed).unwrap();
        let reparsed = reparsed(&file);

        assert_survives(&file.render(), &[PERSONAL_COMMENT, NAME_COMMENT]);
        assert_eq!(names(&reparsed), [PERSONAL, TEAM, API]);
        assert_eq!(reparsed.get(PROVIDER).unwrap().accounts[0], changed);
    }

    #[test]
    fn the_first_account_needs_no_provider_header() {
        let mut file = ProvidersFile::default();

        file.upsert_account(PROVIDER, &account(PERSONAL)).unwrap();

        assert_eq!(
            file.render(),
            "[[anthropic.accounts]]\nname = \"personal\"\n"
        );
        assert!(!reparsed(&file).config.defines_provider(PROVIDER));
    }

    #[test]
    fn upsert_refuses_an_invalid_account() {
        let mut file = edited();

        let error = file
            .upsert_account(PROVIDER, &account(AUTO_SELECTION))
            .unwrap_err();

        assert!(error.to_string().contains(RESERVED_NAME_MESSAGE), "{error}");
        assert_eq!(file.render(), EDITED_FILE);
    }

    #[test]
    fn removing_accounts_keeps_the_rest_of_the_file() {
        let mut file = edited();

        assert!(file.remove_account(PROVIDER, TEAM));
        assert!(!file.remove_account(PROVIDER, TEAM));
        let rendered = file.render();

        assert!(!rendered.contains(TEAM_COMMENT), "{rendered}");
        assert_survives(
            &rendered,
            &[PROVIDER_COMMENT, PERSONAL_COMMENT, OTHER_TABLE],
        );
        assert_eq!(names(&reparsed(&file)), [PERSONAL, API]);
    }

    #[test]
    fn removing_the_last_account_drops_a_table_it_created() {
        let mut file = ProvidersFile::default();
        file.upsert_account(PROVIDER, &account(PERSONAL)).unwrap();

        assert!(file.remove_account(PROVIDER, PERSONAL));

        assert_eq!(file.render(), "");
        assert!(file.get(PROVIDER).is_none());
    }

    #[test]
    fn renaming_keeps_the_entry_and_its_comments() {
        let mut file = edited();

        file.rename_account(PROVIDER, PERSONAL, ADDED).unwrap();
        let rendered = file.render();

        assert_survives(&rendered, &[PERSONAL_COMMENT, NAME_COMMENT]);
        assert_eq!(names(&reparsed(&file)), [ADDED, TEAM, API]);
    }

    #[test_case(TEAM, API, ACCOUNT_EXISTS_MESSAGE ; "taken_name")]
    #[test_case(ADDED, TEAM, UNKNOWN_ACCOUNT_MESSAGE ; "unknown_account")]
    #[test_case(TEAM, AUTO_SELECTION, RESERVED_NAME_MESSAGE ; "reserved_name")]
    fn renaming_refuses(from: &str, to: &str, expected: &str) {
        let mut file = edited();

        let error = file.rename_account(PROVIDER, from, to).unwrap_err();

        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(file.render(), EDITED_FILE);
    }

    #[test_case(API, -1, &[PERSONAL, API, TEAM], true ; "up_one")]
    #[test_case(PERSONAL, 1, &[TEAM, PERSONAL, API], true ; "down_one")]
    #[test_case(PERSONAL, 10, &[TEAM, API, PERSONAL], true ; "clamped_to_the_end")]
    #[test_case(API, -10, &[API, PERSONAL, TEAM], true ; "clamped_to_the_front")]
    #[test_case(PERSONAL, -1, &[PERSONAL, TEAM, API], false ; "already_first")]
    #[test_case(ADDED, 1, &[PERSONAL, TEAM, API], false ; "unknown_account")]
    fn moving_reorders_the_rendered_entries(
        name: &str,
        offset: isize,
        expected: &[&str],
        moved: bool,
    ) {
        let mut file = edited();

        assert_eq!(file.move_account(PROVIDER, name, offset), moved);
        let rendered = file.render();

        assert_survives(&rendered, &KEPT);
        assert_eq!(names(&file), expected);
        assert_eq!(names(&reparsed(&file)), expected);
        assert!(
            rendered.find(PROXY_COMMENT) < rendered.find(SWITCHING_COMMENT)
                && rendered.find(API_COMMENT) < rendered.find(OTHER_TABLE),
            "{rendered}"
        );
    }

    #[test]
    fn moving_carries_the_comment_above_an_entry() {
        let mut file = edited();

        file.move_account(PROVIDER, API, -2);
        let rendered = file.render();

        assert!(
            rendered.find(API_COMMENT) < rendered.find(PERSONAL_COMMENT),
            "{rendered}"
        );
    }

    #[test]
    fn switching_writes_only_what_differs_from_the_defaults() {
        let mut file = edited();
        let switching = SwitchingDef {
            strategy: SwitchStrategy::Paced,
            windows: Some(vec![QuotaWindowName::FiveHour, QuotaWindowName::Weekly]),
            ..SwitchingDef::default()
        };

        file.set_switching(PROVIDER, &switching).unwrap();
        let rendered = file.render();

        assert_survives(&rendered, &KEPT);
        assert!(
            rendered.contains("windows = [\"5h\", \"weekly\"]"),
            "{rendered}"
        );
        assert!(!rendered.contains("switch_below"), "{rendered}");
        assert_eq!(
            reparsed(&file).get(PROVIDER).unwrap().switching,
            Some(switching)
        );
    }

    #[test]
    fn default_switching_removes_the_table() {
        let mut file = edited();

        file.set_switching(PROVIDER, &SwitchingDef::default())
            .unwrap();
        let rendered = file.render();

        assert!(!rendered.contains(SWITCHING_KEY), "{rendered}");
        assert_survives(
            &rendered,
            &[PROVIDER_COMMENT, PERSONAL_COMMENT, OTHER_TABLE],
        );
        assert_eq!(reparsed(&file).get(PROVIDER).unwrap().switching, None);
    }

    #[test]
    fn switching_on_a_slug_the_file_lacks() {
        let mut file = edited();
        let switching = SwitchingDef {
            switch_below: DEFAULT_SWITCH_BELOW + 1,
            ..SwitchingDef::default()
        };

        file.set_switching(UNLISTED, &switching).unwrap();
        let rendered = file.render();

        assert_survives(&rendered, &KEPT);
        assert!(
            rendered.ends_with(&format!(
                "\n[{UNLISTED}.{SWITCHING_KEY}]\nswitch_below = {}\n",
                switching.switch_below
            )),
            "{rendered}"
        );
        let reparsed = reparsed(&file);
        assert_eq!(reparsed.get(UNLISTED).unwrap().switching, Some(switching));
        assert!(!reparsed.config.defines_provider(UNLISTED));
    }

    #[test]
    fn switching_refuses_crossed_thresholds() {
        let mut file = edited();
        let switching = SwitchingDef {
            switch_below: DEFAULT_PREFER_BELOW,
            ..SwitchingDef::default()
        };

        let error = file.set_switching(PROVIDER, &switching).unwrap_err();

        assert!(error.to_string().contains(THRESHOLDS_MESSAGE), "{error}");
    }

    #[test]
    fn the_provider_order_parses_ahead_of_the_providers() {
        let config: ProvidersConfig =
            parse_config("[switching]\nproviders = [\"anthropic\", \"openai\"]\n").unwrap();

        assert_eq!(config.provider_order(), [PROVIDER, OTHER_PROVIDER]);
        assert!(config.get(SWITCHING_KEY).is_none());
    }

    #[test_case("[switching]\nproviders = [\"anthropic\"]\n", SHORT_PROVIDER_ORDER_MESSAGE ; "one_provider")]
    #[test_case("[switching]\nproviders = []\n", SHORT_PROVIDER_ORDER_MESSAGE ; "no_provider")]
    #[test_case("[switching]\nproviders = [\"openai\", \"xai\", \"openai\"]\n", DUPLICATE_PROVIDER_MESSAGE ; "duplicate_provider")]
    #[test_case("[switching]\nproviders = [\"openai\", \" \"]\n", BLANK_PROVIDER_MESSAGE ; "blank_provider")]
    #[test_case("[switching]\nproviders = [\"openai\", \"xai\"]\nstrategy = \"paced\"\n", UNKNOWN_STRATEGY_FIELD_MESSAGE ; "unknown_field")]
    fn invalid_provider_order_fails(text: &str, expected: &str) {
        let error = parse_error(text);

        assert!(error.contains(expected), "{error}");
    }

    #[test]
    fn setting_the_provider_order_keeps_the_file() {
        let mut file = edited();
        let order = [OTHER_PROVIDER.to_owned(), PROVIDER.to_owned()];

        file.set_provider_order(&order).unwrap();
        let rendered = file.render();

        assert_survives(&rendered, &KEPT);
        assert!(
            rendered.ends_with(&format!(
                "\n[{SWITCHING_KEY}]\nproviders = [\"{OTHER_PROVIDER}\", \"{PROVIDER}\"]\n"
            )),
            "{rendered}"
        );
        assert_eq!(reparsed(&file).provider_order(), order);
    }

    #[test]
    fn reordering_keeps_the_comment_on_the_order() {
        let text = format!(
            "{EDITED_FILE}\n{ORDER_COMMENT}\n[switching]\nproviders = [\"anthropic\", \"openai\"]\n"
        );
        let mut file = ProvidersFile::parse(&text).unwrap();
        let order = [
            OTHER_PROVIDER.to_owned(),
            UNLISTED.to_owned(),
            PROVIDER.to_owned(),
        ];

        file.set_provider_order(&order).unwrap();

        assert_survives(&file.render(), &[ORDER_COMMENT]);
        assert_eq!(reparsed(&file).provider_order(), order);
    }

    #[test]
    fn an_empty_provider_order_drops_the_table() {
        let mut file = edited();
        file.set_provider_order(&[PROVIDER.to_owned(), OTHER_PROVIDER.to_owned()])
            .unwrap();

        file.set_provider_order(&[]).unwrap();

        assert_eq!(file.render(), EDITED_FILE);
        assert!(file.provider_order().is_empty());
    }

    #[test]
    fn a_short_provider_order_is_refused() {
        let mut file = edited();

        let error = file.set_provider_order(&[PROVIDER.to_owned()]).unwrap_err();

        assert!(
            error.to_string().contains(SHORT_PROVIDER_ORDER_MESSAGE),
            "{error}"
        );
        assert_eq!(file.render(), EDITED_FILE);
    }

    #[test]
    fn a_provider_edit_leaves_its_accounts_as_written() {
        let mut file = edited();
        let mut def = file.get(PROVIDER).unwrap().clone();
        def.default_model = Some(format!("{PROVIDER}/{ADDED}"));

        file.upsert(PROVIDER.into(), def).unwrap();

        assert_survives(&file.render(), &KEPT);
        assert_survives(&file.render(), &[NAME_COMMENT]);
    }
}
