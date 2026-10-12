//! Which of this server's own environment variables a prepared shell command receives.
//!
//! A client names patterns; values are resolved here, against this process's environment, and
//! never inside the ceiling the operator configured. The server's own configuration is withheld
//! whatever either side asks for: its HTTP bearer token would let a command call this server's
//! tools directly, around the client's permission review.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
};

use workcell_host_contract::{
    ContractVersion, EnvPattern, EnvironmentEntry, EnvironmentEntryState, EnvironmentListResponse,
    MAX_ENVIRONMENT_ENTRIES, MAX_ENVIRONMENT_RESPONSE_BYTES, ShellEnvironmentCapability,
    ShellEnvironmentLimits, ShellEnvironmentSelection,
};
use workcell_mcp_shell::ShellEnvironment;
use workcell_tool_contract::shell_env::{
    BASE_ALLOWLIST, EnvPatternError, is_proxy_variable, is_server_variable,
    is_shell_startup_variable,
};

pub const CEILING_ENV: &str = "WORKCELL_MCP_SHELL_ENV_CEILING";
const PATTERN_SEPARATOR: char = ',';

/// `None` for an empty value, which leaves forwarding off. The ceiling follows the rules of a
/// selection, so it can never admit a name that decides how Bash starts.
pub fn parse_ceiling(text: &str) -> Result<Option<Vec<EnvPattern>>, EnvPatternError> {
    if text.is_empty() {
        return Ok(None);
    }
    let ceiling = ShellEnvironmentSelection {
        pass: text
            .split(PATTERN_SEPARATOR)
            .map(EnvPattern::parse)
            .collect::<Result<_, _>>()?,
        set: BTreeMap::new(),
    };
    ceiling.validate()?;
    Ok(Some(ceiling.pass))
}

#[derive(Clone, Debug)]
pub struct ShellForwarding {
    ceiling: Vec<EnvPattern>,
    /// Names the server read its configuration from, decided once at startup.
    server_variables: BTreeSet<String>,
}

impl ShellForwarding {
    #[must_use]
    pub const fn new(ceiling: Vec<EnvPattern>, server_variables: BTreeSet<String>) -> Self {
        Self {
            ceiling,
            server_variables,
        }
    }

    #[must_use]
    pub fn ceiling(&self) -> &[EnvPattern] {
        &self.ceiling
    }

    #[must_use]
    pub fn capability(&self) -> ShellEnvironmentCapability {
        ShellEnvironmentCapability {
            version: ContractVersion::V1,
            ceiling: self.ceiling.clone(),
            list: true,
            limits: ShellEnvironmentLimits {
                max_entries: MAX_ENVIRONMENT_ENTRIES,
                max_response_bytes: MAX_ENVIRONMENT_RESPONSE_BYTES,
            },
        }
    }

    /// Proxies are stripped too: every command already receives them through the allowlist, so a
    /// selection can neither add nor replace them.
    fn is_stripped(&self, name: &str) -> bool {
        is_server_variable(name)
            || is_shell_startup_variable(name)
            || is_proxy_variable(name)
            || self.server_variables.contains(name)
    }

    fn state(&self, selection: &ShellEnvironmentSelection, name: &str) -> EnvironmentEntryState {
        if selection.set.contains_key(name) {
            EnvironmentEntryState::Set
        } else if BASE_ALLOWLIST.contains(&name) {
            EnvironmentEntryState::Allowlisted
        } else if self.is_stripped(name) {
            EnvironmentEntryState::Stripped
        } else if !self.ceiling.iter().any(|pattern| pattern.matches(name)) {
            EnvironmentEntryState::OutsideCeiling
        } else if selection.pass.iter().any(|pattern| pattern.matches(name)) {
            EnvironmentEntryState::Forwarded
        } else {
            EnvironmentEntryState::Available
        }
    }

    /// The overlay for one prepared command: forwarded variables, then the selection's literals.
    /// The selection must already be validated.
    pub fn resolve(
        &self,
        selection: &ShellEnvironmentSelection,
        variables: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> ShellEnvironment {
        let mut resolved = utf8_names(variables)
            .filter(|(name, _)| self.state(selection, name) == EnvironmentEntryState::Forwarded)
            .collect::<BTreeMap<_, _>>();
        resolved.extend(
            selection
                .set
                .iter()
                .map(|(name, value)| (name.clone(), OsString::from(value))),
        );
        resolved.into_iter().collect()
    }

    /// Every variable and what the selection would do with it, sorted by name and bounded.
    pub fn list(
        &self,
        selection: &ShellEnvironmentSelection,
        variables: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> EnvironmentListResponse {
        let mut entries = utf8_names(variables)
            .filter(|(name, _)| !selection.set.contains_key(name))
            .map(|(name, value)| {
                let state = self.state(selection, &name);
                let value = match state {
                    EnvironmentEntryState::Allowlisted
                    | EnvironmentEntryState::Forwarded
                    | EnvironmentEntryState::Available => {
                        Some(value.to_string_lossy().into_owned())
                    }
                    EnvironmentEntryState::Set
                    | EnvironmentEntryState::OutsideCeiling
                    | EnvironmentEntryState::Stripped => None,
                };
                (name.clone(), EnvironmentEntry { name, value, state })
            })
            .collect::<BTreeMap<_, _>>();
        entries.extend(selection.set.iter().map(|(name, value)| {
            (
                name.clone(),
                EnvironmentEntry {
                    name: name.clone(),
                    value: Some(value.clone()),
                    state: EnvironmentEntryState::Set,
                },
            )
        }));
        let mut response = EnvironmentListResponse {
            version: ContractVersion::V1,
            entries: Vec::new(),
            truncated: false,
        };
        let mut bytes = 0_u64;
        for entry in entries.into_values() {
            bytes = bytes.saturating_add(serde_json::to_vec(&entry).map_or(u64::MAX, |encoded| {
                u64::try_from(encoded.len()).unwrap_or(u64::MAX)
            }));
            if response.entries.len() >= MAX_ENVIRONMENT_ENTRIES as usize
                || bytes > MAX_ENVIRONMENT_RESPONSE_BYTES
            {
                response.truncated = true;
                break;
            }
            response.entries.push(entry);
        }
        response
    }
}

fn utf8_names(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> impl Iterator<Item = (String, OsString)> {
    variables
        .into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const CANARY: &str = "FREE_CANARY";
    const CANARY_VALUE: &str = "canary-value";
    const TOKEN: &str = "WORKCELL_MCP_HTTP_TOKEN";
    const FILE_KEY: &str = "OPERATOR_FILE_KEY";
    const CONSULTED: &str = "OPERATOR_CONSULTED";
    const SECRET_VALUE: &str = "server-secret";
    const PREFIXED: &str = "FOO_ONE";
    const UNPREFIXED: &str = "BAR_ONE";
    const LITERAL: &str = "literal";

    fn forwarding(ceiling: &str) -> ShellForwarding {
        ShellForwarding::new(
            parse_ceiling(ceiling).unwrap().unwrap(),
            BTreeSet::from([FILE_KEY.to_owned(), CONSULTED.to_owned()]),
        )
    }

    fn variables() -> Vec<(OsString, OsString)> {
        [
            (CANARY, CANARY_VALUE),
            (TOKEN, SECRET_VALUE),
            (FILE_KEY, SECRET_VALUE),
            (CONSULTED, SECRET_VALUE),
            ("SEARXNG_BEARER_TOKEN", SECRET_VALUE),
            ("BASH_ENV", SECRET_VALUE),
            ("HTTP_PROXY", "http://proxy"),
            ("PATH", "/bin"),
            (PREFIXED, CANARY_VALUE),
            (UNPREFIXED, CANARY_VALUE),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect()
    }

    fn names(environment: &ShellEnvironment) -> Vec<&str> {
        environment.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[test_case("", Ok(None) ; "empty_is_off")]
    #[test_case("*", Ok(Some(vec![EnvPattern::All])) ; "everything")]
    #[test_case("FOO_*,BAR", Ok(Some(vec![EnvPattern::Prefix("FOO_".into()), EnvPattern::Exact("BAR".into())])) ; "list")]
    #[test_case("FOO, BAR", Err(EnvPatternError::Invalid(" BAR".into())) ; "spaces")]
    #[test_case("FOO,", Err(EnvPatternError::Empty) ; "trailing_separator")]
    #[test_case("BASH_*", Err(EnvPatternError::Reserved("BASH*".into())) ; "reserved")]
    fn ceiling(text: &str, expected: Result<Option<Vec<EnvPattern>>, EnvPatternError>) {
        assert_eq!(parse_ceiling(text), expected);
    }

    #[test]
    fn everything_forwards_only_what_is_not_server_configuration() {
        let resolved =
            forwarding("*").resolve(&ShellEnvironmentSelection::everything(), variables());
        assert_eq!(names(&resolved), [UNPREFIXED, PREFIXED, CANARY]);
    }

    #[test]
    fn the_ceiling_bounds_what_a_selection_forwards() {
        let resolved =
            forwarding("FOO_*").resolve(&ShellEnvironmentSelection::everything(), variables());
        assert_eq!(names(&resolved), [PREFIXED]);
    }

    #[test]
    fn a_literal_wins_over_a_forwarded_value() {
        let selection = ShellEnvironmentSelection {
            pass: vec![EnvPattern::All],
            set: BTreeMap::from([(PREFIXED.to_owned(), LITERAL.to_owned())]),
        };
        let resolved = forwarding("FOO_*").resolve(&selection, variables());
        assert_eq!(
            resolved.as_ref(),
            [(PREFIXED.to_owned(), OsString::from(LITERAL))]
        );
    }

    #[test_case(CANARY, EnvironmentEntryState::Available, Some(CANARY_VALUE) ; "available")]
    #[test_case(PREFIXED, EnvironmentEntryState::Forwarded, Some(CANARY_VALUE) ; "forwarded")]
    #[test_case(UNPREFIXED, EnvironmentEntryState::Set, Some(LITERAL) ; "set")]
    #[test_case("PATH", EnvironmentEntryState::Allowlisted, Some("/bin") ; "allowlisted")]
    #[test_case(TOKEN, EnvironmentEntryState::Stripped, None ; "token")]
    #[test_case(FILE_KEY, EnvironmentEntryState::Stripped, None ; "file_key")]
    #[test_case(CONSULTED, EnvironmentEntryState::Stripped, None ; "consulted")]
    #[test_case("BASH_ENV", EnvironmentEntryState::Stripped, None ; "startup")]
    fn list_classifies(name: &str, state: EnvironmentEntryState, value: Option<&str>) {
        let selection = ShellEnvironmentSelection {
            pass: vec![EnvPattern::parse("FOO_*").unwrap()],
            set: BTreeMap::from([(UNPREFIXED.to_owned(), LITERAL.to_owned())]),
        };
        let response = forwarding("*").list(&selection, variables());
        let entry = response
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap();
        assert_eq!(
            (entry.state, entry.value.as_deref()),
            (state, value),
            "{name}"
        );
        assert!(!response.truncated);
    }

    #[test]
    fn list_withholds_values_outside_the_ceiling_and_sorts_by_name() {
        let response = forwarding("FOO_*").list(&ShellEnvironmentSelection::default(), variables());
        let canary = response
            .entries
            .iter()
            .find(|entry| entry.name == CANARY)
            .unwrap();
        assert_eq!(
            (canary.state, canary.value.as_deref()),
            (EnvironmentEntryState::OutsideCeiling, None)
        );
        assert!(
            response
                .entries
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
        );
        assert!(
            response
                .entries
                .iter()
                .all(|entry| entry.value.as_deref() != Some(SECRET_VALUE))
        );
    }

    #[test]
    fn list_is_bounded() {
        let variables = (0..=MAX_ENVIRONMENT_ENTRIES)
            .map(|index| (format!("VAR_{index:05}").into(), OsString::new()))
            .collect::<Vec<_>>();
        let response = forwarding("*").list(&ShellEnvironmentSelection::default(), variables);
        assert!(response.truncated);
        assert_eq!(response.entries.len(), MAX_ENVIRONMENT_ENTRIES as usize);
    }
}
