//! One row of `/env`, whichever tab built it, and how a value is shown before it is revealed.

use std::fmt::Display;

use caudra_agent::secrets::SecretScope;

use crate::components::escape_terminal_controls;
use crate::components::list_picker::PickerItem;

pub(super) const PATTERN_SECTION: &str = "Pass patterns";
pub(super) const FORWARDED_SECTION: &str = "Forwarded";
pub(super) const BUILT_IN_STATE: &str = "built-in";
pub(super) const PASS_STATE: &str = "pass";
pub(super) const INVALID_STATE: &str = "invalid";
pub(super) const BUILT_IN_NOTE: &str =
    "Every shell command receives this from the fixed allowlist.";
pub(super) const MASK: &str = "****";
pub(super) const EMPTY_VALUE: &str = "(empty)";
const ELLIPSIS: char = '…';
const MASK_EDGE_CHARS: usize = 4;
const MASK_SHORT_CHARS: usize = 8;
const VIA_PREFIX: &str = "via ";

/// How turning a forwarded row off rewrites the pass list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Unforward {
    /// Drops the row's own exact entry.
    Exact,
    /// Replaces `*` with every other forwarded name.
    Everything,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RowAction {
    None,
    Forward,
    Unforward(Unforward),
    SetEntry,
    Pattern(String),
    /// A secret by name; its value never reaches a row.
    Secret(SecretScope),
}

pub(super) struct EnvRow {
    pub(super) name: String,
    pub(super) section: &'static str,
    badge: Option<&'static str>,
    value: Option<String>,
    detail: String,
    revealed: bool,
    state: Option<String>,
    pub(super) action: RowAction,
    pub(super) checked: bool,
    inert: bool,
    pub(super) note: Option<String>,
    /// The info area as it reads with this row selected, which only sizes it.
    info: Option<String>,
}

impl EnvRow {
    /// A variable whose value stays masked until revealed.
    pub(super) fn variable(section: &'static str, name: &str, value: &str) -> Self {
        Self {
            value: Some(escape_terminal_controls(value)),
            detail: masked(value),
            ..Self::entry(section, name, String::new())
        }
    }

    /// A row with nothing to reveal, its detail shown as given.
    pub(super) fn entry(section: &'static str, name: &str, detail: String) -> Self {
        Self {
            name: escape_terminal_controls(name),
            section,
            badge: None,
            value: None,
            detail,
            revealed: false,
            state: None,
            action: RowAction::None,
            checked: false,
            inert: false,
            note: None,
            info: None,
        }
    }

    pub(super) fn badge(mut self, badge: &'static str) -> Self {
        self.badge = Some(badge);
        self
    }

    pub(super) fn state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    pub(super) fn action(mut self, action: RowAction) -> Self {
        self.action = action;
        self
    }

    pub(super) fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub(super) fn checked(mut self) -> Self {
        self.checked = true;
        self
    }

    pub(super) fn inert(mut self) -> Self {
        self.inert = true;
        self
    }

    /// The info area shows `header`, then the selected row's note: the tallest pair sizes it.
    pub(super) fn size_info(&mut self, header: &str) {
        self.info = self.note.as_ref().map(|note| format!("{header}\n{note}"));
    }

    pub(super) fn toggleable(&self) -> bool {
        matches!(self.action, RowAction::Forward | RowAction::Unforward(_))
    }

    pub(super) fn revealable(&self) -> bool {
        self.value.is_some()
    }

    pub(super) fn reveal(&mut self) {
        self.revealed = self.value.is_some();
    }

    /// What identifies a row across a rebuild, since one name can sit in two sections.
    pub(super) fn identity(&self) -> (&'static str, &str) {
        (self.section, &self.name)
    }

    pub(super) fn is_variable(&self) -> bool {
        !matches!(self.action, RowAction::SetEntry | RowAction::Pattern(_))
    }
}

impl PickerItem for EnvRow {
    fn label(&self) -> &str {
        &self.name
    }

    fn detail(&self) -> Option<&str> {
        let shown = match (&self.value, self.revealed) {
            (Some(value), true) => value,
            _ => &self.detail,
        };
        (!shown.is_empty()).then_some(shown.as_str())
    }

    fn badge(&self) -> Option<&str> {
        self.badge
    }

    fn suffix(&self) -> Option<&str> {
        self.state.as_deref()
    }

    fn section(&self) -> Option<&str> {
        Some(self.section)
    }

    fn is_disabled(&self) -> bool {
        self.inert
    }

    fn info(&self) -> Option<&str> {
        self.info.as_deref()
    }
}

/// `first4…last4`, or a fixed mask for a value too short to cut without showing most of it.
pub(super) fn masked(value: &str) -> String {
    if value.is_empty() {
        return EMPTY_VALUE.into();
    }
    let count = value.chars().count();
    if count <= MASK_SHORT_CHARS {
        return MASK.into();
    }
    let head: String = value.chars().take(MASK_EDGE_CHARS).collect();
    let tail: String = value.chars().skip(count - MASK_EDGE_CHARS).collect();
    escape_terminal_controls(&format!("{head}{ELLIPSIS}{tail}"))
}

pub(super) fn via(pattern: impl Display) -> String {
    format!("{VIA_PREFIX}{pattern}")
}

pub(super) fn via_note(pattern: impl Display) -> String {
    format!(
        "Forwarded by {pattern}. Select that entry under {PATTERN_SECTION} and press d to stop forwarding these names."
    )
}

pub(super) fn names_state(count: usize) -> String {
    match count {
        1 => "1 name".into(),
        count => format!("{count} names"),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("", EMPTY_VALUE ; "empty")]
    #[test_case("short", MASK ; "short")]
    #[test_case("exactly8", MASK ; "at_the_cut")]
    #[test_case("sk-live-1234567890", "sk-l…7890" ; "long")]
    fn values_are_masked(value: &str, expected: &str) {
        assert_eq!(masked(value), expected);
    }

    #[test]
    fn a_revealed_row_shows_its_whole_value() {
        let mut row = EnvRow::variable(FORWARDED_SECTION, "TOKEN", "sk-live-1234567890");
        assert_eq!(row.detail(), Some("sk-l…7890"));
        row.reveal();
        assert_eq!(row.detail(), Some("sk-live-1234567890"));
    }
}
