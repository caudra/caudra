//! The Sandbox tab: the guest's variables as its server lists them, and the selection a sandbox
//! session asks it to forward. Values resolve in the guest; only names and literals leave here.

use caudra_workcell::host_contract::{EnvironmentEntry, EnvironmentEntryState};
use workcell_tool_contract::shell_env::{
    EnvPattern, EnvPatternError, ShellEnvironmentSelection, looks_secret, validate_literal,
};

use super::Change;
use super::rows::{
    BUILT_IN_NOTE, BUILT_IN_STATE, EnvRow, FORWARDED_SECTION, PASS_STATE, PATTERN_SECTION,
    RowAction, Unforward, names_state, via, via_note,
};

pub(super) const PROFILE_SET_SECTION: &str = "Set by profile";
pub(super) const AVAILABLE_SECTION: &str = "Available";
pub(super) const OUTSIDE_SECTION: &str = "Outside the server ceiling";
pub(super) const STRIPPED_SECTION: &str = "Withheld by the guest server";
pub(super) const STRIPPED_DETAIL: &str = "Workcell's own configuration and credentials";
pub(super) const SELECTION_BADGE: &str = "selection";
pub(super) const GUEST_BADGE: &str = "guest";
const EVERYTHING_NOTE: &str =
    "Forwarded by *. Turning it off switches to an explicit list of the other forwarded names.";
const OUTSIDE_NOTE: &str = "The guest image's ceiling does not admit this name.";
const STRIPPED_NOTE: &str =
    "The guest server never forwards its own configuration or credentials, nor shows them.";

/// The guest's listing under the selection this session applies.
pub(crate) struct GuestListing {
    pub(crate) selection: ShellEnvironmentSelection,
    pub(crate) entries: Vec<EnvironmentEntry>,
    pub(crate) truncated: bool,
}

impl GuestListing {
    fn forwarded(&self) -> impl Iterator<Item = &EnvironmentEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.state == EnvironmentEntryState::Forwarded)
    }

    /// The first credential-looking name `pattern` would start forwarding.
    pub(super) fn sensitive_match(&self, pattern: &EnvPattern) -> Option<String> {
        if let Some(name) = pattern.exact() {
            return looks_secret(name).then(|| name.to_owned());
        }
        self.entries
            .iter()
            .find(|entry| {
                entry.state == EnvironmentEntryState::Available
                    && pattern.matches(&entry.name)
                    && looks_secret(&entry.name)
            })
            .map(|entry| entry.name.clone())
    }

    /// The selection after `change`, checked the way the guest will check it.
    pub(super) fn edited(&self, change: Change) -> Result<ShellEnvironmentSelection, String> {
        let mut selection = self.selection.clone();
        match change {
            Change::Forward(name) => selection.pass.push(EnvPattern::Exact(name)),
            Change::Unforward(name, Unforward::Exact) => selection
                .pass
                .retain(|pattern| pattern.exact() != Some(name.as_str())),
            Change::Unforward(name, Unforward::Everything) => {
                selection.pass = self
                    .forwarded()
                    .filter(|entry| entry.name != name)
                    .map(|entry| EnvPattern::Exact(entry.name.clone()))
                    .collect();
            }
            Change::SetValue { name, value } => {
                validate_literal(&name, &value).map_err(|error| error.to_string())?;
                selection.set.insert(name, value);
            }
            Change::RemoveValue(name) => {
                selection.set.remove(&name);
            }
            Change::AddPattern(pattern) => {
                if let Some(name) = pattern.reserved_capture() {
                    return Err(EnvPatternError::Reserved(name).to_string());
                }
                if !selection.pass.contains(&pattern) {
                    selection.pass.push(pattern);
                }
            }
            Change::RemovePattern(raw) => {
                selection.pass.retain(|pattern| pattern.to_string() != raw)
            }
        }
        selection.validate().map_err(|error| error.to_string())?;
        Ok(selection)
    }
}

pub(super) fn sandbox_rows(listing: &GuestListing) -> Vec<EnvRow> {
    let selection = &listing.selection;
    let mut rows: Vec<EnvRow> = selection
        .set
        .iter()
        .map(|(name, value)| {
            EnvRow::variable(PROFILE_SET_SECTION, name, value)
                .badge(SELECTION_BADGE)
                .action(RowAction::SetEntry)
                .checked()
        })
        .collect();
    rows.extend(selection.pass.iter().map(|pattern| {
        let raw = pattern.to_string();
        let matched = listing
            .forwarded()
            .filter(|entry| pattern.matches(&entry.name))
            .count();
        EnvRow::entry(PATTERN_SECTION, &raw, String::new())
            .badge(SELECTION_BADGE)
            .state(names_state(matched))
            .action(RowAction::Pattern(raw))
            .checked()
    }));
    let everything = selection.pass.contains(&EnvPattern::All);
    let mut forwarded = Vec::new();
    let mut available = Vec::new();
    let mut outside = Vec::new();
    let mut stripped = Vec::new();
    for entry in &listing.entries {
        let name = &entry.name;
        let value = entry.value.as_deref().unwrap_or_default();
        match entry.state {
            EnvironmentEntryState::Set => {}
            EnvironmentEntryState::Allowlisted => forwarded.push(
                EnvRow::variable(FORWARDED_SECTION, name, value)
                    .badge(GUEST_BADGE)
                    .state(BUILT_IN_STATE)
                    .note(BUILT_IN_NOTE)
                    .checked(),
            ),
            EnvironmentEntryState::Forwarded => {
                let row = EnvRow::variable(FORWARDED_SECTION, name, value)
                    .badge(GUEST_BADGE)
                    .checked();
                let prefix = selection.pass.iter().find(|pattern| {
                    matches!(pattern, EnvPattern::Prefix(_)) && pattern.matches(name)
                });
                forwarded.push(if everything {
                    row.state(via(EnvPattern::All))
                        .note(EVERYTHING_NOTE)
                        .action(RowAction::Unforward(Unforward::Everything))
                } else if let Some(prefix) = prefix {
                    row.state(via(prefix)).note(via_note(prefix))
                } else {
                    row.state(PASS_STATE)
                        .action(RowAction::Unforward(Unforward::Exact))
                });
            }
            EnvironmentEntryState::Available => available.push(
                EnvRow::variable(AVAILABLE_SECTION, name, value)
                    .badge(GUEST_BADGE)
                    .action(RowAction::Forward),
            ),
            EnvironmentEntryState::OutsideCeiling => outside.push(
                EnvRow::entry(OUTSIDE_SECTION, name, String::new())
                    .note(OUTSIDE_NOTE)
                    .inert(),
            ),
            EnvironmentEntryState::Stripped => stripped.push(
                EnvRow::entry(STRIPPED_SECTION, name, STRIPPED_DETAIL.into())
                    .note(STRIPPED_NOTE)
                    .inert(),
            ),
        }
    }
    rows.extend(forwarded);
    rows.extend(available);
    rows.extend(outside);
    rows.extend(stripped);
    rows
}
