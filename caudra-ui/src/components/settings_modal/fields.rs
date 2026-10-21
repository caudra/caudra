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
const MAX_PREFIX: &str = "Maximum: ";
const WRITTEN: &str = "Set in the global caudra.toml. d resets it to the default.";
const NOT_WRITTEN: &str = "Not set, so the default applies.";
const PROJECT_MARK: &str = "  project";
pub(super) const OVERRIDDEN: &str =
    "This project's .caudra/caudra.toml sets this key too, and its value wins here.";
const NOT_A_NUMBER: &str = "Enter a whole number, or nothing to reset it.";
const BELOW_MIN: &str = "Enter a whole number no lower than ";
const ABOVE_MAX: &str = "Enter a whole number no higher than ";
const SENTENCE_END: &str = ".";

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

    /// The value `text` spells for `field`, `None` for empty text, which resets the key.
    pub(super) fn parse(self, field: &ConfigField, text: &str) -> Result<Option<Value>, String> {
        if text.is_empty() {
            return Ok(None);
        }
        match self {
            Self::Integer => {
                let number = text.parse::<i64>().map_err(|_| NOT_A_NUMBER.to_owned())?;
                let below = field
                    .min
                    .filter(|min| i128::from(number) < i128::from(*min))
                    .map(|min| (BELOW_MIN, min));
                let above = field
                    .max
                    .filter(|max| i128::from(number) > i128::from(*max))
                    .map(|max| (ABOVE_MAX, max));
                match below.or(above) {
                    Some((prefix, bound)) => Err(format!("{prefix}{bound}{SENTENCE_END}")),
                    None => Ok(Some(Value::from(number))),
                }
            }
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
    overridden: bool,
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
    if overridden {
        spans.push(Span::styled(PROJECT_MARK, theme.tool_warning));
    }
    spans
}

pub(super) fn field_info(
    field: &ConfigField,
    table: &[&str],
    written: Option<&Item>,
    overridden: bool,
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
    if let Some(max) = field.max {
        lines.push(Line::styled(format!("{MAX_PREFIX}{max}"), theme.tool_dim));
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
    if overridden {
        lines.push(Line::styled(OVERRIDDEN, theme.tool_warning));
    }
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

    const LOWEST: u64 = 2;
    const HIGHEST: u64 = 20;
    const BOUNDED: ConfigField = ConfigField {
        name: "bounded",
        ty: "integer",
        default: ConfigValue::Unset,
        min: Some(LOWEST),
        max: Some(HIGHEST),
        env: None,
        description: "",
    };

    #[test_case(ScalarKind::Integer, "12", Some("12"); "integer")]
    #[test_case(ScalarKind::Integer, "2", Some("2"); "the_lowest_allowed")]
    #[test_case(ScalarKind::Integer, "20", Some("20"); "the_highest_allowed")]
    #[test_case(ScalarKind::Text, "dark", Some("\"dark\""); "text")]
    #[test_case(ScalarKind::Text, "", None; "empty_resets")]
    fn text_parses_by_kind(kind: ScalarKind, text: &str, expected: Option<&str>) {
        let parsed = kind.parse(&BOUNDED, text).unwrap();
        assert_eq!(
            parsed.map(|value| value.to_string().trim().to_owned()),
            expected.map(str::to_owned)
        );
    }

    #[test_case("ten", NOT_A_NUMBER.to_owned(); "a_word")]
    #[test_case("1", format!("{BELOW_MIN}{LOWEST}{SENTENCE_END}"); "below_the_minimum")]
    #[test_case("-1", format!("{BELOW_MIN}{LOWEST}{SENTENCE_END}"); "negative")]
    #[test_case("21", format!("{ABOVE_MAX}{HIGHEST}{SENTENCE_END}"); "above_the_maximum")]
    fn a_number_out_of_bounds_is_refused(text: &str, expected: String) {
        assert_eq!(
            ScalarKind::Integer.parse(&BOUNDED, text).unwrap_err(),
            expected
        );
    }
}
