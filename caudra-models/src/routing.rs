use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde::Deserialize;

use crate::validate::pointer_token;
use crate::{CatalogProvider, Issue, Provider};

pub(crate) const HEADING: &str = "## Routing changes\n\nA catalog row's `api`, `env`, and `npm` decide where Caudra sends a request and which key it sends. Confirm each change on the provider's own page, then accept it in `audit/accepted.json` under `routing`, or `make check` fails. A protected row belongs to a built-in provider or shares a built-in key. Caudra keeps its bundled routing for those, so only a person accepts their changes.\n";
pub(crate) const NONE: &str = "No routing changes.";
pub(crate) const NOT_COMPARED: &str =
    "Not compared: pass --published with the published models.json.";
pub(crate) const UNACCEPTED_MESSAGE: &str = "routing changed since the published file; confirm it on the provider's page and accept it under routing in audit/accepted.json";
pub(crate) const PROTECTED_MESSAGE: &str = "protected routing changed since the published file; Caudra keeps its bundled routing for this row, so only a person may accept it under routing in audit/accepted.json";
const ABSENT: &str = "-";
const ENV_SEPARATOR: &str = ", ";

type Catalog = BTreeMap<String, CatalogProvider>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingField {
    Api,
    Env,
    Npm,
}

impl RoutingField {
    const ALL: [Self; 3] = [Self::Api, Self::Env, Self::Npm];

    pub fn label(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Env => "env",
            Self::Npm => "npm",
        }
    }

    fn value(self, row: Option<&CatalogProvider>) -> String {
        let value = row.and_then(|row| match self {
            Self::Api => row.api.clone(),
            Self::Env => (!row.env.is_empty()).then(|| row.env.join(ENV_SEPARATOR)),
            Self::Npm => Some(row.npm.clone()),
        });
        value.unwrap_or_else(|| ABSENT.to_owned())
    }
}

/// A reviewed routing change. It silences the change only while the catalog
/// prints exactly `catalog`, so a later change reports again.
#[derive(Debug, Clone, Deserialize)]
pub struct RoutingAccepted {
    /// The catalog slug, such as `opencode`.
    pub provider: String,
    pub field: RoutingField,
    /// The new value as the report prints it, `-` for a removed value.
    pub catalog: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingChange {
    pub provider: String,
    pub field: RoutingField,
    pub before: String,
    pub after: String,
    pub protected: bool,
}

impl RoutingChange {
    pub fn matches(&self, accepted: &RoutingAccepted) -> bool {
        self.provider == accepted.provider
            && self.field == accepted.field
            && self.after == accepted.catalog
    }

    pub fn issue(&self) -> Issue {
        let message = if self.protected {
            PROTECTED_MESSAGE
        } else {
            UNACCEPTED_MESSAGE
        };
        Issue {
            pointer: format!(
                "/catalog/{}/{}",
                pointer_token(&self.provider),
                self.field.label()
            ),
            message: format!("{message}: {} -> {}", self.before, self.after),
        }
    }
}

/// Every `api`, `env`, or `npm` value that differs between two catalogs,
/// including rows one of them lacks. `builtin` holds the built-in providers'
/// catalog slugs.
pub fn changes(
    builtin: &BTreeSet<String>,
    before: &Catalog,
    after: &Catalog,
) -> Vec<RoutingChange> {
    let protected = protected_rows(builtin, [before, after]);
    let slugs: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    slugs
        .into_iter()
        .flat_map(|slug| {
            RoutingField::ALL.into_iter().filter_map(|field| {
                let (old, new) = (field.value(before.get(slug)), field.value(after.get(slug)));
                (old != new).then(|| RoutingChange {
                    provider: slug.clone(),
                    field,
                    before: old,
                    after: new,
                    protected: protected.contains(slug.as_str()),
                })
            })
        })
        .collect()
}

/// Rows named by a built-in catalog slug, plus rows that list one of those
/// rows' environment keys in either catalog.
fn protected_rows<'a>(builtin: &BTreeSet<String>, catalogs: [&'a Catalog; 2]) -> BTreeSet<&'a str> {
    let rows = || catalogs.into_iter().flatten();
    let keys: BTreeSet<&str> = rows()
        .filter(|(slug, _)| builtin.contains(*slug))
        .flat_map(|(_, row)| row.env.iter().map(String::as_str))
        .collect();
    rows()
        .filter(|(slug, row)| {
            builtin.contains(*slug) || row.env.iter().any(|key| keys.contains(key.as_str()))
        })
        .map(|(slug, _)| slug.as_str())
        .collect()
}

pub fn builtin_slugs<'a>(providers: impl IntoIterator<Item = &'a Provider>) -> BTreeSet<String> {
    providers
        .into_iter()
        .filter_map(|provider| provider.catalog_slug.clone())
        .collect()
}

/// The report section, or `NOT_COMPARED` without a baseline.
pub fn section(changes: Option<&[RoutingChange]>) -> String {
    let mut text = format!("{HEADING}\n");
    match changes {
        None => text.push_str(NOT_COMPARED),
        Some([]) => text.push_str(NONE),
        Some(changes) => {
            text.push_str(
                "| Provider | Field | Published | Now | Protected |\n|---|---|---|---|---|",
            );
            for change in changes {
                let _ = write!(
                    text,
                    "\n| {} | {} | {} | {} | {} |",
                    change.provider,
                    change.field.label(),
                    change.before,
                    change.after,
                    if change.protected { "yes" } else { "no" }
                );
            }
        }
    }
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::json;
    use test_case::test_case;

    use super::{ABSENT, NONE, NOT_COMPARED, RoutingAccepted, RoutingField, changes, section};
    use crate::CatalogProvider;

    const BUILTIN: &str = "opencode";
    const SHARED: &str = "opencode-mirror";
    const OTHER: &str = "gateway";
    const BUILTIN_KEY: &str = "OPENCODE_API_KEY";
    const OTHER_KEY: &str = "GATEWAY_API_KEY";
    const API: &str = "https://gateway.example.com/v1";
    const MOVED_API: &str = "https://moved.example.com/v1";
    const NPM: &str = "@ai-sdk/openai-compatible";

    fn row(env: &str, api: &str) -> CatalogProvider {
        serde_json::from_value(json!({"name": "Row", "env": [env], "npm": NPM, "api": api}))
            .unwrap()
    }

    fn catalog(rows: &[(&str, CatalogProvider)]) -> BTreeMap<String, CatalogProvider> {
        rows.iter()
            .map(|(slug, row)| ((*slug).to_owned(), row.clone()))
            .collect()
    }

    fn builtin() -> BTreeSet<String> {
        BTreeSet::from([BUILTIN.to_owned()])
    }

    #[test_case(BUILTIN, BUILTIN_KEY, true; "builtin_slug")]
    #[test_case(SHARED, BUILTIN_KEY, true; "shares_a_builtin_key")]
    #[test_case(OTHER, OTHER_KEY, false; "unrelated_row")]
    fn a_moved_api_is_a_change_protected_by_slug_or_key(slug: &str, key: &str, protected: bool) {
        let base = [(BUILTIN, row(BUILTIN_KEY, API))];
        let before = catalog(&[base[0].clone(), (slug, row(key, API))]);
        let after = catalog(&[base[0].clone(), (slug, row(key, MOVED_API))]);
        let found = changes(&builtin(), &before, &after);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (
                found[0].provider.as_str(),
                found[0].field,
                found[0].before.as_str(),
                found[0].after.as_str(),
                found[0].protected
            ),
            (slug, RoutingField::Api, API, MOVED_API, protected)
        );
    }

    #[test]
    fn an_added_row_reports_every_routing_field() {
        let after = catalog(&[(OTHER, row(OTHER_KEY, API))]);
        let found = changes(&builtin(), &BTreeMap::new(), &after);
        assert_eq!(
            found
                .iter()
                .map(|change| (change.field, change.before.as_str(), change.after.as_str()))
                .collect::<Vec<_>>(),
            [
                (RoutingField::Api, ABSENT, API),
                (RoutingField::Env, ABSENT, OTHER_KEY),
                (RoutingField::Npm, ABSENT, NPM),
            ]
        );
    }

    #[test]
    fn identical_catalogs_have_no_changes() {
        let rows = catalog(&[(BUILTIN, row(BUILTIN_KEY, API))]);
        assert!(changes(&builtin(), &rows, &rows).is_empty());
    }

    #[test_case(MOVED_API, true; "same_value")]
    #[test_case(API, false; "other_value")]
    fn an_acceptance_matches_only_the_printed_value(catalog_value: &str, matches: bool) {
        let before = catalog(&[(OTHER, row(OTHER_KEY, API))]);
        let after = catalog(&[(OTHER, row(OTHER_KEY, MOVED_API))]);
        let accepted = RoutingAccepted {
            provider: OTHER.to_owned(),
            field: RoutingField::Api,
            catalog: catalog_value.to_owned(),
            reason: OTHER.to_owned(),
        };
        assert_eq!(
            changes(&builtin(), &before, &after)[0].matches(&accepted),
            matches
        );
    }

    #[test_case(None, NOT_COMPARED; "without_a_baseline")]
    #[test_case(Some(&[]), NONE; "without_changes")]
    fn an_empty_section_says_why(changes: Option<&[super::RoutingChange]>, expected: &str) {
        assert!(section(changes).contains(expected));
    }
}
