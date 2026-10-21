//! The plain tabs: each scalar key of one `caudra.toml` table, as the global file writes it or
//! as its default. Arrays, tables, and keys that take more than one type are left to the file.

use caudra_config::{ConfigField, ConfigValue};
use ratatui::text::{Line, Span};
use toml_edit::{Item, Value};

use crate::components::escape_terminal_controls;
use crate::theme::Theme;

const BOOL_TYPES: [&str; 2] = ["bool", "boolean"];
const INTEGER_TYPES: [&str; 4] = ["integer", "u32", "u64", "usize"];
const TEXT_TYPES: [&str; 2] = ["string", "String"];
const DEFAULT_SUFFIX: &str = " (default)";
const NAME_GAP: usize = 2;
const TABLE_SEPARATOR: &str = ".";
const DEFAULT_PREFIX: &str = "Default: ";
const ENV_PREFIX: &str = "Environment variable: ";
const MIN_PREFIX: &str = "Minimum: ";
const WRITTEN: &str = "Set in the global caudra.toml. d resets it to the default.";
const NOT_WRITTEN: &str = "Not set, so the default applies.";
pub(super) const NOT_A_NUMBER: &str = "Enter a whole number, or nothing to reset it.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScalarKind {
    Bool,
    Integer,
    Text,
}

impl ScalarKind {
    pub(super) fn of(field: &ConfigField) -> Option<Self> {
        if BOOL_TYPES.contains(&field.ty) {
            Some(Self::Bool)
        } else if INTEGER_TYPES.contains(&field.ty) {
            Some(Self::Integer)
        } else if TEXT_TYPES.contains(&field.ty) {
            Some(Self::Text)
        } else {
            None
        }
    }

    /// The value `text` spells, `None` for empty text, which resets the key.
    pub(super) fn parse(self, text: &str) -> Result<Option<Value>, &'static str> {
        if text.is_empty() {
            return Ok(None);
        }
        match self {
            Self::Integer => text
                .parse::<i64>()
                .map(|number| Some(Value::from(number)))
                .map_err(|_| NOT_A_NUMBER),
            Self::Bool | Self::Text => Ok(Some(Value::from(text))),
        }
    }
}

/// The keys of `fields` a tab can change in place.
pub(super) fn scalar_fields(fields: &'static [ConfigField]) -> Vec<&'static ConfigField> {
    fields
        .iter()
        .filter(|field| ScalarKind::of(field).is_some())
        .collect()
}

/// A written value as text, a string without its quotes.
pub(super) fn written_text(item: &Item) -> String {
    match item.as_value() {
        Some(Value::String(text)) => text.value().clone(),
        Some(value) => value.to_string().trim().to_owned(),
        None => item.to_string().trim().to_owned(),
    }
}

/// The value a boolean key holds now, written or by default.
pub(super) fn current_bool(field: &ConfigField, written: Option<&Item>) -> bool {
    written
        .and_then(Item::as_bool)
        .unwrap_or(matches!(field.default, ConfigValue::Bool(true)))
}

pub(super) fn field_row(
    field: &ConfigField,
    written: Option<&Item>,
    name_cols: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let name = format!("{:<width$}", field.name, width = name_cols + NAME_GAP);
    let mut spans = vec![Span::raw(name)];
    match written {
        Some(item) => spans.push(Span::styled(
            escape_terminal_controls(&written_text(item)),
            theme.accent,
        )),
        None => {
            spans.push(Span::styled(field.default.format_default(), theme.tool_dim));
            spans.push(Span::styled(DEFAULT_SUFFIX, theme.tool_dim));
        }
    }
    spans
}

pub(super) fn field_info(
    field: &ConfigField,
    table: &[&str],
    written: Option<&Item>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let path = table
        .iter()
        .copied()
        .chain([field.name])
        .collect::<Vec<_>>()
        .join(TABLE_SEPARATOR);
    let mut lines = vec![
        Line::styled(path, theme.bold),
        Line::styled(field.description, theme.item_desc),
        Line::styled(
            format!("{DEFAULT_PREFIX}{}", field.default.format_default()),
            theme.tool_dim,
        ),
    ];
    if let Some(min) = field.min {
        lines.push(Line::styled(format!("{MIN_PREFIX}{min}"), theme.tool_dim));
    }
    if let Some(env) = field.env {
        lines.push(Line::styled(format!("{ENV_PREFIX}{env}"), theme.tool_dim));
    }
    lines.push(Line::styled(
        match written {
            Some(_) => WRITTEN,
            None => NOT_WRITTEN,
        },
        theme.tool_dim,
    ));
    lines
}

/// The widest key name among `fields`, which the value column starts after.
pub(super) fn name_cols(fields: &[&ConfigField]) -> usize {
    fields
        .iter()
        .map(|field| field.name.len())
        .max()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use caudra_config::{TOP_LEVEL_FIELDS, UiConfig};
    use test_case::test_case;

    use super::*;

    const ALWAYS_THINKING: &str = "always_thinking";
    const ALWAYS_COLLAPSED: &str = "always_collapsed";

    #[test_case(TOP_LEVEL_FIELDS, ALWAYS_THINKING; "a_key_of_two_types")]
    #[test_case(UiConfig::FIELDS, ALWAYS_COLLAPSED; "an_array")]
    fn only_scalar_keys_are_listed(fields: &'static [ConfigField], skipped: &str) {
        let listed = scalar_fields(fields);
        assert!(!listed.is_empty());
        assert!(listed.iter().all(|field| field.name != skipped));
    }

    #[test_case(ScalarKind::Integer, "12", Some("12"); "integer")]
    #[test_case(ScalarKind::Text, "dark", Some("\"dark\""); "text")]
    #[test_case(ScalarKind::Text, "", None; "empty_resets")]
    fn text_parses_by_kind(kind: ScalarKind, text: &str, expected: Option<&str>) {
        let parsed = kind.parse(text).unwrap();
        assert_eq!(
            parsed.map(|value| value.to_string().trim().to_owned()),
            expected.map(str::to_owned)
        );
    }

    #[test]
    fn a_word_is_not_a_number() {
        assert_eq!(ScalarKind::Integer.parse("ten").unwrap_err(), NOT_A_NUMBER);
    }
}
