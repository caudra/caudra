//! The Local tab: what Caudra can see, what `[agent.shell_env]` forwards of it, and why the rest
//! stays behind.

use std::collections::{BTreeMap, HashSet};

use caudra_agent::herdr::PANE_ENVIRONMENT;
use caudra_config::shell_env::GlobalShellEnvFile;
use caudra_config::{EnvSource, env_source, shell_visible_environment};
use caudra_providers::catalog_providers_if_available;
use workcell_tool_contract::shell_env::{
    BASE_ALLOWLIST, EnvPattern, is_shell_startup_variable, is_valid_variable_name, looks_secret,
};

use crate::components::escape_terminal_controls;

use super::rows::{
    BUILT_IN_NOTE, BUILT_IN_STATE, ENV_REFERENCE, EnvRow, FORWARDED_SECTION, INVALID_STATE,
    PASS_STATE, PATTERN_SECTION, RowAction, Unforward, names_state, via, via_note,
};

pub(super) const SET_SECTION: &str = "Set by config";
pub(super) const NOT_FORWARDED_SECTION: &str = "Not forwarded";
pub(super) const NOT_FORWARDABLE_SECTION: &str = "Not forwardable";
pub(super) const CONFIG_BADGE: &str = "config";
pub(super) const PROCESS_BADGE: &str = "process";
pub(super) const GLOBAL_ENV_BADGE: &str = "global .env";
pub(super) const PROJECT_ENV_BADGE: &str = "project .env";
pub(super) const HERDR_STATE: &str = "Herdr pane";
pub(super) const SET_STATE: &str = "set";
pub(super) const UNSET_STATE: &str = "unset";
pub(super) const PROJECT_STATE: &str = "repository";
pub(super) const STARTUP_STATE: &str = "Bash startup";
pub(super) const PROJECT_NOTE: &str =
    "Set by this project's .env; repositories cannot choose forwarded values.";
pub(super) const STARTUP_NOTE: &str =
    "Decides how Bash starts, so no shell command can receive it.";
const INVALID_NAME_NOTE: &str = "Not a name a shell variable can have.";
const HERDR_NOTE: &str = "Herdr's pane variables reach every command run in a Herdr pane.";

pub(crate) struct HostVariable {
    pub(crate) value: String,
    pub(crate) source: EnvSource,
}

/// The global config as written and every variable this process sees, with the provider
/// credential names a forward should ask about first.
pub(crate) struct LocalEnvironment {
    pub(crate) file: GlobalShellEnvFile,
    /// [`EnvSource::ProjectEnvFile`] marks a name only the project's `.env` set, which no
    /// shell command receives.
    pub(crate) variables: BTreeMap<String, HostVariable>,
    pub(crate) credentials: HashSet<String>,
}

impl LocalEnvironment {
    pub(crate) fn capture(file: GlobalShellEnvFile) -> Self {
        let mut variables: BTreeMap<String, HostVariable> = std::env::vars_os()
            .filter_map(|(name, value)| {
                Some((
                    name.into_string().ok()?,
                    HostVariable {
                        value: value.to_string_lossy().into_owned(),
                        source: EnvSource::ProjectEnvFile,
                    },
                ))
            })
            .collect();
        for (name, value) in shell_visible_environment() {
            let source = match env_source(&name) {
                EnvSource::GlobalEnvFile => EnvSource::GlobalEnvFile,
                EnvSource::Process | EnvSource::ProjectEnvFile => EnvSource::Process,
            };
            let value = value.to_string_lossy().into_owned();
            variables.insert(name, HostVariable { value, source });
        }
        let credentials = catalog_providers_if_available()
            .into_iter()
            .flatten()
            .flat_map(|provider| provider.env_keys)
            .collect();
        Self {
            file,
            variables,
            credentials,
        }
    }

    pub(super) fn is_sensitive(&self, name: &str) -> bool {
        looks_secret(name) || self.credentials.contains(name)
    }

    /// Whether `name` would reach a shell command if a pattern matched it.
    pub(super) fn is_forwardable(&self, name: &str) -> bool {
        !is_shell_startup_variable(name)
            && self
                .variables
                .get(name)
                .is_some_and(|variable| variable.source != EnvSource::ProjectEnvFile)
    }

    /// The first sensitive name `pattern` would forward, the name itself for an exact entry.
    pub(super) fn sensitive_match(&self, pattern: &EnvPattern) -> Option<String> {
        if let Some(name) = pattern.exact() {
            return self.is_sensitive(name).then(|| name.to_owned());
        }
        self.variables
            .keys()
            .find(|name| {
                pattern.matches(name) && self.is_forwardable(name) && self.is_sensitive(name)
            })
            .cloned()
    }
}

pub(super) fn local_rows(environment: &LocalEnvironment) -> Vec<EnvRow> {
    let file = &environment.file;
    let patterns: Vec<EnvPattern> = file
        .pass()
        .iter()
        .filter_map(|raw| EnvPattern::parse(raw).ok())
        .collect();
    let mut rows: Vec<EnvRow> = file
        .set()
        .iter()
        .map(|(name, value)| set_row(name, value))
        .collect();
    rows.extend(file.pass().iter().map(|raw| pattern_row(environment, raw)));
    let mut forwarded = Vec::new();
    let mut not_forwarded = Vec::new();
    let mut withheld = Vec::new();
    for (name, variable) in &environment.variables {
        if file.set().contains_key(name) {
            continue;
        }
        let badge = match variable.source {
            EnvSource::Process => PROCESS_BADGE,
            EnvSource::GlobalEnvFile => GLOBAL_ENV_BADGE,
            EnvSource::ProjectEnvFile => PROJECT_ENV_BADGE,
        };
        let row = |section| EnvRow::variable(section, name, &variable.value).badge(badge);
        let refused = if !is_valid_variable_name(name) {
            Some((INVALID_STATE, INVALID_NAME_NOTE))
        } else if variable.source == EnvSource::ProjectEnvFile {
            Some((PROJECT_STATE, PROJECT_NOTE))
        } else if is_shell_startup_variable(name) {
            Some((STARTUP_STATE, STARTUP_NOTE))
        } else {
            None
        };
        if let Some((state, note)) = refused {
            withheld.push(row(NOT_FORWARDABLE_SECTION).state(state).note(note).inert());
            continue;
        }
        let fixed = if BASE_ALLOWLIST.contains(&name.as_str()) {
            Some((BUILT_IN_STATE, BUILT_IN_NOTE))
        } else if PANE_ENVIRONMENT.contains(&name.as_str()) {
            Some((HERDR_STATE, HERDR_NOTE))
        } else {
            None
        };
        if let Some((state, note)) = fixed {
            forwarded.push(row(FORWARDED_SECTION).state(state).note(note).checked());
            continue;
        }
        let prefix = patterns
            .iter()
            .find(|pattern| pattern.exact().is_none() && pattern.matches(name));
        if let Some(prefix) = prefix {
            forwarded.push(
                row(FORWARDED_SECTION)
                    .state(via(prefix))
                    .note(via_note(prefix))
                    .checked(),
            );
        } else if patterns.iter().any(|pattern| pattern.exact() == Some(name)) {
            forwarded.push(
                row(FORWARDED_SECTION)
                    .state(PASS_STATE)
                    .action(RowAction::Unforward(Unforward::Exact))
                    .checked(),
            );
        } else {
            not_forwarded.push(row(NOT_FORWARDED_SECTION).action(RowAction::Forward));
        }
    }
    rows.extend(forwarded);
    rows.extend(not_forwarded);
    rows.extend(withheld);
    rows
}

/// A `{env:NAME}` reference names where the value comes from, so it is shown as written.
fn set_row(name: &str, value: &str) -> EnvRow {
    let row = match value.contains(ENV_REFERENCE) {
        true => EnvRow::entry(SET_SECTION, name, escape_terminal_controls(value)),
        false => EnvRow::variable(SET_SECTION, name, value),
    };
    row.badge(CONFIG_BADGE)
        .action(RowAction::SetEntry)
        .checked()
}

fn pattern_row(environment: &LocalEnvironment, raw: &str) -> EnvRow {
    let state = match EnvPattern::parse(raw) {
        Ok(EnvPattern::Exact(name)) => match environment.is_forwardable(&name) {
            true => SET_STATE.to_owned(),
            false => UNSET_STATE.to_owned(),
        },
        Ok(pattern) => names_state(
            environment
                .variables
                .keys()
                .filter(|name| pattern.matches(name) && environment.is_forwardable(name))
                .count(),
        ),
        Err(_) => INVALID_STATE.to_owned(),
    };
    EnvRow::entry(PATTERN_SECTION, raw, String::new())
        .badge(CONFIG_BADGE)
        .state(state)
        .action(RowAction::Pattern(raw.to_owned()))
        .checked()
}
