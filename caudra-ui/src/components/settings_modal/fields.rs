//! The plain tabs: each key of one or more `caudra.toml` tables a tab can change in place, as
//! the global file writes it or as its default. Tables of their own and keys that take more than
//! one type are left to the file.

use std::ops::Range;

use caudra_config::{ConfigField, ConfigValue};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use toml_edit::{Array, DocumentMut, Item, Value};
use unicode_width::UnicodeWidthStr;

use crate::components::{escape_terminal_controls, hover_style};
use crate::event_loop::config_reload::ApplyTiming;
use crate::theme::Theme;

const BOOL_TYPES: [&str; 2] = ["bool", "boolean"];
const INTEGER_TYPES: [&str; 4] = ["integer", "u32", "u64", "usize"];
const FLOAT_TYPES: [&str; 1] = ["f64"];
const TEXT_TYPES: [&str; 2] = ["string", "String"];
const LIST_TYPES: [&str; 1] = ["string[]"];
/// The key a list default is read back under, as the TOML it is documented in.
const DEFAULT_LIST_KEY: &str = "default";
const DEFAULT_SUFFIX: &str = " (default)";
const NAME_GAP: usize = 2;
const CHIP_GAP: &str = "  ";
pub(super) const LIST_ITEM_SEPARATOR: &str = ", ";
const EMPTY_LIST: &str = "[]";
pub(super) const TABLE_SEPARATOR: &str = ".";
const DEFAULT_PREFIX: &str = "Default: ";
const CHOICES_PREFIX: &str = "Choices: ";
const ENV_PREFIX: &str = "Environment variable: ";
const MIN_PREFIX: &str = "Minimum: ";
const MAX_PREFIX: &str = "Maximum: ";
const APPLIES_PREFIX: &str = "Applies ";
const WRITTEN: &str = "Set in the global caudra.toml. d resets it to the default.";
const NOT_WRITTEN: &str = "Not set, so the default applies.";
const PROJECT_MARK: &str = "  project";
pub(super) const DOCS_BADGE: &str = "[?]";
pub(super) const DOCS_BADGE_GAP: &str = " ";
pub(super) const OVERRIDDEN: &str =
    "This project's .caudra/caudra.toml sets this key too, and its value wins here.";
const NOT_A_NUMBER: &str = "Enter a whole number, or nothing to reset it.";
const NOT_A_DECIMAL: &str = "Enter a number such as 2.5, or nothing to reset it.";
const BELOW_MIN: &str = "Enter a number no lower than ";
const ABOVE_MAX: &str = "Enter a number no higher than ";
const SENTENCE_END: &str = ".";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FieldKind {
    Bool,
    Integer,
    Float,
    Text,
    /// A string from [`ConfigField::choices`].
    Choice,
    /// A list of strings, each from [`ConfigField::choices`] when it lists any.
    List,
}

impl FieldKind {
    pub(super) fn of(field: &ConfigField) -> Option<Self> {
        if LIST_TYPES.contains(&field.ty) {
            Some(Self::List)
        } else if !field.choices.is_empty() {
            Some(Self::Choice)
        } else if BOOL_TYPES.contains(&field.ty) {
            Some(Self::Bool)
        } else if INTEGER_TYPES.contains(&field.ty) {
            Some(Self::Integer)
        } else if FLOAT_TYPES.contains(&field.ty) {
            Some(Self::Float)
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
                bounded(field, number as f64).map(|()| Some(Value::from(number)))
            }
            Self::Float => {
                let number = text
                    .parse::<f64>()
                    .ok()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| NOT_A_DECIMAL.to_owned())?;
                bounded(field, number).map(|()| Some(Value::from(number)))
            }
            Self::Bool | Self::Text | Self::Choice | Self::List => Ok(Some(Value::from(text))),
        }
    }
}

fn bounded(field: &ConfigField, number: f64) -> Result<(), String> {
    let below = field
        .min
        .filter(|min| number < *min as f64)
        .map(|min| (BELOW_MIN, min));
    let above = field
        .max
        .filter(|max| number > *max as f64)
        .map(|max| (ABOVE_MAX, max));
    match below.or(above) {
        Some((prefix, bound)) => Err(format!("{prefix}{bound}{SENTENCE_END}")),
        None => Ok(()),
    }
}

/// The keys of `fields` a tab can change in place.
pub(super) fn editable_fields(
    fields: &'static [ConfigField],
) -> impl Iterator<Item = &'static ConfigField> {
    fields.iter().filter(|field| FieldKind::of(field).is_some())
}

/// A written value as text, a string without its quotes and a list as its items.
pub(super) fn written_text(item: &Item) -> String {
    match item.as_value() {
        Some(Value::String(text)) => text.value().clone(),
        Some(Value::Array(array)) => array
            .iter()
            .map(|value| match value {
                Value::String(text) => text.value().clone(),
                value => value.to_string().trim().to_owned(),
            })
            .collect::<Vec<_>>()
            .join(LIST_ITEM_SEPARATOR),
        Some(value) => value.to_string().trim().to_owned(),
        None => item.to_string().trim().to_owned(),
    }
}

pub(super) fn current_bool(field: &ConfigField, written: Option<&Item>) -> bool {
    written
        .and_then(Item::as_bool)
        .unwrap_or(matches!(field.default, ConfigValue::Bool(true)))
}

/// The choice in force: the written one, else the default.
pub(super) fn current_choice(field: &ConfigField, written: Option<&Item>) -> Option<&'static str> {
    let wanted = match (written, field.default) {
        (Some(written), _) => written.as_str(),
        (None, ConfigValue::Str(default)) => Some(default),
        (None, _) => None,
    }?;
    field
        .choices
        .iter()
        .copied()
        .find(|choice| *choice == wanted)
}

/// The choice after the current one, round to the first.
pub(super) fn next_choice(field: &ConfigField, written: Option<&Item>) -> &'static str {
    let next = current_choice(field, written)
        .and_then(|current| field.choices.iter().position(|choice| *choice == current))
        .map_or(0, |index| (index + 1) % field.choices.len());
    field.choices[next]
}

/// The items of a written list, `None` while the key is unset or not a list of strings.
fn written_list(written: Option<&Item>) -> Option<Vec<String>> {
    written?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

/// The items a list key holds now: the written ones, else its default.
pub(super) fn current_list(field: &ConfigField, written: Option<&Item>) -> Vec<String> {
    written_list(written).unwrap_or_else(|| match field.default {
        ConfigValue::Toml(text) => format!("{DEFAULT_LIST_KEY} = {text}")
            .parse::<DocumentMut>()
            .ok()
            .and_then(|document| written_list(document.get(DEFAULT_LIST_KEY)))
            .unwrap_or_default(),
        _ => Vec::new(),
    })
}

pub(super) fn list_value(items: &[String]) -> Value {
    Value::Array(items.iter().map(String::as_str).collect::<Array>())
}

/// A key row: its name, then what it holds. A choice key lists every choice, with the columns
/// each spans from the row's start, so a click picks one.
pub(super) struct FieldRow {
    pub(super) spans: Vec<Span<'static>>,
    pub(super) chips: Vec<(Range<u16>, &'static str)>,
}

pub(super) fn field_row(
    field: &ConfigField,
    written: Option<&Item>,
    overridden: bool,
    name_cols: usize,
    theme: &Theme,
) -> FieldRow {
    let name = format!("{:<width$}", field.name, width = name_cols + NAME_GAP);
    let mut spans = vec![Span::raw(name)];
    let mut chips = Vec::new();
    match (FieldKind::of(field), written) {
        (Some(FieldKind::Choice), _) => {
            let current = current_choice(field, written);
            for (index, choice) in field.choices.iter().copied().enumerate() {
                if index > 0 {
                    spans.push(Span::raw(CHIP_GAP));
                }
                let style = match current == Some(choice) {
                    true => theme
                        .accent
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    false => theme.tool_dim,
                };
                let start = spans_width(&spans);
                spans.push(Span::styled(choice, style));
                chips.push((start..spans_width(&spans), choice));
            }
            if written.is_none() {
                spans.push(Span::styled(DEFAULT_SUFFIX, theme.tool_dim));
            }
        }
        (_, Some(item)) => spans.push(Span::styled(
            escape_terminal_controls(&shown_value(item)),
            theme.accent,
        )),
        (_, None) => {
            spans.push(Span::styled(field.default.format_default(), theme.tool_dim));
            spans.push(Span::styled(DEFAULT_SUFFIX, theme.tool_dim));
        }
    }
    if overridden {
        spans.push(Span::styled(PROJECT_MARK, theme.tool_warning));
    }
    FieldRow { spans, chips }
}

fn shown_value(item: &Item) -> String {
    match item.as_array().is_some_and(Array::is_empty) {
        true => EMPTY_LIST.to_owned(),
        false => written_text(item),
    }
}

fn spans_width(spans: &[Span]) -> u16 {
    let width: usize = spans.iter().map(|span| span.content.width()).sum();
    u16::try_from(width).unwrap_or(u16::MAX)
}

/// `table.key`, or the key alone at the top level.
pub(super) fn key_path(table: &[&str], field: &ConfigField) -> String {
    table
        .iter()
        .copied()
        .chain([field.name])
        .collect::<Vec<_>>()
        .join(TABLE_SEPARATOR)
}

/// What the info pane says about a key besides its metadata.
pub(super) struct FieldState<'a> {
    pub(super) written: Option<&'a Item>,
    pub(super) overridden: bool,
    pub(super) timing: ApplyTiming,
    pub(super) badge_hovered: bool,
}

/// The key's path with its docs badge first, then its description and metadata.
pub(super) fn field_info(
    field: &ConfigField,
    table: &[&str],
    state: FieldState<'_>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(key_path(table, field), theme.bold),
            Span::raw(DOCS_BADGE_GAP),
            Span::styled(
                DOCS_BADGE,
                hover_style(theme.keybind_key, state.badge_hovered),
            ),
        ]),
        Line::styled(field.description, theme.item_desc),
        Line::styled(
            format!("{DEFAULT_PREFIX}{}", field.default.format_default()),
            theme.tool_dim,
        ),
    ];
    if !field.choices.is_empty() {
        lines.push(Line::styled(
            format!(
                "{CHOICES_PREFIX}{}",
                field.choices.join(LIST_ITEM_SEPARATOR)
            ),
            theme.tool_dim,
        ));
    }
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
        format!("{APPLIES_PREFIX}{}{SENTENCE_END}", state.timing.suffix()),
        theme.tool_dim,
    ));
    lines.push(Line::styled(
        match state.written {
            Some(_) => WRITTEN,
            None => NOT_WRITTEN,
        },
        theme.tool_dim,
    ));
    if state.overridden {
        lines.push(Line::styled(OVERRIDDEN, theme.tool_warning));
    }
    lines
}

/// The widest key name among `fields`, which the value column starts after.
pub(super) fn name_cols<'a>(fields: impl IntoIterator<Item = &'a ConfigField>) -> usize {
    fields
        .into_iter()
        .map(|field| field.name.len())
        .max()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use caudra_config::{AgentConfig, TOP_LEVEL_FIELDS, UiConfig};
    use test_case::test_case;

    use super::*;

    const ALWAYS_THINKING: &str = "always_thinking";
    const TOOL_OUTPUT_LINES: &str = "tool_output_lines";
    const CACHE_WARMING: &str = "cache_warming";
    const LEARNING_TARGETS: &str = "learning_targets";

    #[test_case(TOP_LEVEL_FIELDS, ALWAYS_THINKING; "a_key_of_two_types")]
    #[test_case(UiConfig::FIELDS, TOOL_OUTPUT_LINES; "a_table")]
    fn keys_a_tab_cannot_edit_are_left_out(fields: &'static [ConfigField], skipped: &str) {
        let listed: Vec<_> = editable_fields(fields).collect();
        assert!(!listed.is_empty());
        assert!(listed.iter().all(|field| field.name != skipped));
    }

    fn agent_field(name: &str) -> &'static ConfigField {
        AgentConfig::FIELDS
            .iter()
            .find(|field| field.name == name)
            .expect("an agent key")
    }

    #[test_case(CACHE_WARMING, FieldKind::Choice; "a_choice")]
    #[test_case(LEARNING_TARGETS, FieldKind::List; "a_list_of_choices")]
    fn kinds_follow_the_metadata(name: &str, expected: FieldKind) {
        assert_eq!(FieldKind::of(agent_field(name)), Some(expected));
    }

    #[test]
    fn the_next_choice_wraps_round() {
        let field = agent_field(CACHE_WARMING);
        let last = field.choices.last().copied().expect("choices");
        let written = Item::Value(Value::from(last));
        assert_eq!(next_choice(field, Some(&written)), field.choices[0]);
        assert_eq!(next_choice(field, None), field.choices[1]);
    }

    #[test]
    fn an_unset_list_holds_its_default() {
        let field = agent_field(LEARNING_TARGETS);
        assert_eq!(current_list(field, None), field.choices);
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
        choices: &[],
        description: "",
    };

    #[test_case(FieldKind::Integer, "12", Some("12"); "integer")]
    #[test_case(FieldKind::Integer, "2", Some("2"); "the_lowest_allowed")]
    #[test_case(FieldKind::Integer, "20", Some("20"); "the_highest_allowed")]
    #[test_case(FieldKind::Float, "2.5", Some("2.5"); "decimal")]
    #[test_case(FieldKind::Text, "dark", Some("\"dark\""); "text")]
    #[test_case(FieldKind::Text, "", None; "empty_resets")]
    fn text_parses_by_kind(kind: FieldKind, text: &str, expected: Option<&str>) {
        let parsed = kind.parse(&BOUNDED, text).unwrap();
        assert_eq!(
            parsed.map(|value| value.to_string().trim().to_owned()),
            expected.map(str::to_owned)
        );
    }

    #[test_case(FieldKind::Integer, "ten", NOT_A_NUMBER.to_owned(); "a_word")]
    #[test_case(FieldKind::Integer, "1", format!("{BELOW_MIN}{LOWEST}{SENTENCE_END}"); "below_the_minimum")]
    #[test_case(FieldKind::Integer, "-1", format!("{BELOW_MIN}{LOWEST}{SENTENCE_END}"); "negative")]
    #[test_case(FieldKind::Integer, "21", format!("{ABOVE_MAX}{HIGHEST}{SENTENCE_END}"); "above_the_maximum")]
    #[test_case(FieldKind::Float, "inf", NOT_A_DECIMAL.to_owned(); "infinite")]
    #[test_case(FieldKind::Float, "1.5", format!("{BELOW_MIN}{LOWEST}{SENTENCE_END}"); "a_decimal_below_the_minimum")]
    fn a_number_out_of_bounds_is_refused(kind: FieldKind, text: &str, expected: String) {
        assert_eq!(kind.parse(&BOUNDED, text).unwrap_err(), expected);
    }
}
