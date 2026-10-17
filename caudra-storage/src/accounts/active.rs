//! Which account each provider uses: one the user pinned, or `auto` to let
//! the switching policy choose. One global state row maps each provider to
//! its pinned name; a provider without an entry is automatic.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{AUTO_SELECTION, AccountName, AccountNameError, validate_provider_slug};
use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir, StorageError};

const ACTIVE: StateKey = StateKey {
    name: "accounts.active",
    class: StateClass::Persistent,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AccountSelection {
    #[default]
    Auto,
    Pinned(AccountName),
}

impl fmt::Display for AccountSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str(AUTO_SELECTION),
            Self::Pinned(name) => write!(formatter, "{name}"),
        }
    }
}

impl FromStr for AccountSelection {
    type Err = AccountNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == AUTO_SELECTION {
            return Ok(Self::Auto);
        }
        AccountName::new(value).map(Self::Pinned)
    }
}

impl TryFrom<String> for AccountSelection {
    type Error = AccountNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<AccountSelection> for String {
    fn from(selection: AccountSelection) -> Self {
        selection.to_string()
    }
}

/// Every provider's selection. An unreadable row reads as all automatic, and
/// an entry that no longer parses is skipped, so a damaged value never keeps
/// a provider from answering.
pub fn read_account_selections(dir: &StateDir) -> BTreeMap<String, AccountSelection> {
    let stored = state::get::<BTreeMap<String, String>>(dir, SCOPE_GLOBAL, ACTIVE)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read the account selections");
            None
        })
        .unwrap_or_default();
    stored
        .into_iter()
        .filter_map(|(provider, value)| match value.parse() {
            Ok(selection) => Some((provider, selection)),
            Err(error) => {
                warn!(%provider, %value, %error, "ignoring an invalid account selection");
                None
            }
        })
        .collect()
}

pub fn read_account_selection(dir: &StateDir, provider: &str) -> AccountSelection {
    read_account_selections(dir)
        .remove(provider)
        .unwrap_or_default()
}

/// [`AccountSelection::Auto`] removes the provider's entry. The error reaches
/// the caller because the user asked for the switch and has to hear when it
/// did not stick.
pub fn persist_account_selection(
    dir: &StateDir,
    provider: &str,
    selection: &AccountSelection,
) -> Result<(), StorageError> {
    validate_provider_slug(provider)?;
    state::update(
        dir,
        SCOPE_GLOBAL,
        ACTIVE,
        |selections: &mut BTreeMap<String, String>| match selection {
            AccountSelection::Auto => {
                selections.remove(provider);
            }
            AccountSelection::Pinned(name) => {
                selections.insert(provider.to_owned(), name.to_string());
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const ANTHROPIC: &str = "anthropic";
    const OPENAI: &str = "openai";
    const PERSONAL: &str = "personal";
    const WORK: &str = "work";
    const ABSENT_IS_AUTO: &str = "a provider with no entry must be automatic";
    const AUTO_DELETES: &str = "choosing auto must remove the provider's entry";

    fn state() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn pinned(name: &str) -> AccountSelection {
        AccountSelection::Pinned(AccountName::new(name).unwrap())
    }

    #[test_case(&[], AccountSelection::Auto ; "absent")]
    #[test_case(&[pinned(PERSONAL)], pinned(PERSONAL) ; "pinned")]
    #[test_case(&[pinned(PERSONAL), pinned(WORK)], pinned(WORK) ; "replaced")]
    #[test_case(&[pinned(PERSONAL), AccountSelection::Auto], AccountSelection::Auto ; "auto")]
    fn selections_round_trip(writes: &[AccountSelection], expected: AccountSelection) {
        let (_temp, dir) = state();
        for selection in writes {
            persist_account_selection(&dir, ANTHROPIC, selection).unwrap();
        }

        assert_eq!(read_account_selection(&dir, ANTHROPIC), expected);
        assert_eq!(
            read_account_selection(&dir, OPENAI),
            AccountSelection::Auto,
            "{ABSENT_IS_AUTO}"
        );
    }

    #[test]
    fn auto_removes_only_its_provider() {
        let (_temp, dir) = state();
        persist_account_selection(&dir, ANTHROPIC, &pinned(PERSONAL)).unwrap();
        persist_account_selection(&dir, OPENAI, &pinned(WORK)).unwrap();

        persist_account_selection(&dir, ANTHROPIC, &AccountSelection::Auto).unwrap();

        assert_eq!(
            read_account_selections(&dir),
            BTreeMap::from([(OPENAI.to_owned(), pinned(WORK))]),
            "{AUTO_DELETES}"
        );
    }

    #[test]
    fn invalid_stored_entries_are_skipped() {
        let (_temp, dir) = state();
        state::set(
            &dir,
            SCOPE_GLOBAL,
            ACTIVE,
            &BTreeMap::from([(ANTHROPIC, "Not A Name"), (OPENAI, PERSONAL)]),
        )
        .unwrap();

        assert_eq!(
            read_account_selections(&dir),
            BTreeMap::from([(OPENAI.to_owned(), pinned(PERSONAL))])
        );
    }

    #[test_case(AUTO_SELECTION, AccountSelection::Auto ; "auto")]
    #[test_case(PERSONAL, pinned(PERSONAL) ; "name")]
    fn selections_serialize_as_plain_strings(text: &str, selection: AccountSelection) {
        let json = serde_json::to_string(&selection).unwrap();
        assert_eq!(json, format!("\"{text}\""));
        assert_eq!(
            serde_json::from_str::<AccountSelection>(&json).unwrap(),
            selection
        );
    }

    #[test]
    fn provider_slugs_are_validated() {
        let (_temp, dir) = state();
        assert!(matches!(
            persist_account_selection(&dir, "../x", &AccountSelection::Auto),
            Err(StorageError::InvalidProviderSlug(_))
        ));
    }
}
