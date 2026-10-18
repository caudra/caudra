//! The answer of a Home Assistant call. An intent says what it did and to
//! which devices; `GetLiveContext`, and a resource such as the Assist context
//! snapshot, list the exposed entities, which read best as a table.

use caudra_agent::{McpBlock, McpOutput};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde::Deserialize;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::{Rows, one_line, resource_label};
use crate::components::command_text::ellipsize_spans;
use crate::theme;

const LIVE_CONTEXT_TOOL: &str = "GetLiveContext";
/// Home Assistant names a tool from an API it merges `api__tool`.
const NAMESPACE_SEPARATOR: &str = "__";
const ERROR_RESPONSE: &str = "error";
const RESPONSE_WORD_SEPARATOR: char = '_';
/// Argument keys that say where an intent acted, in the order they are named.
const SCOPE_KEYS: &[&str] = &["domain", "device_class", "area", "floor"];
const DETAIL_SEPARATOR: &str = " · ";
const LIST_SEPARATOR: &str = ", ";
const SUCCEEDED_SIGIL: &str = "✓ ";
const FAILED_SIGIL: &str = "✗ ";
const LIVE_CONTEXT_RESULT_KEY: &str = "result";
const ENTITY_MARKER: &str = "- ";
const FIELD_INDENT: &str = "  ";
const KEY_SEPARATOR: &str = ": ";
const NAMES_KEY: &str = "names";
const DOMAIN_KEY: &str = "domain";
const STATE_KEY: &str = "state";
const AREAS_KEY: &str = "areas";
/// How Home Assistant joins an entity's aliases, and an area's.
const ALIAS_SEPARATOR: &str = ", ";
const TABLE_HEADINGS: [&str; 4] = ["name", "domain", "state", "area"];
const COLUMN_GAP: &str = "  ";
const MAX_COLUMN_WIDTH: usize = 32;
const SINGLE_QUOTE: char = '\'';
const DOUBLE_QUOTE: char = '"';
const ESCAPED_SINGLE_QUOTE: &str = "''";
const SINGLE_QUOTE_TEXT: &str = "'";

/// An intent response as Home Assistant's `IntentResponse.as_dict` writes
/// it, less the language and card the LLM API drops.
#[derive(Deserialize)]
struct IntentResponse {
    #[serde(default)]
    speech: Speeches,
    response_type: String,
    #[serde(default)]
    data: IntentData,
}

#[derive(Default, Deserialize)]
struct Speeches {
    plain: Option<Speech>,
}

#[derive(Deserialize)]
struct Speech {
    #[serde(default)]
    speech: String,
}

#[derive(Default, Deserialize)]
struct IntentData {
    #[serde(default)]
    success: Vec<Target>,
    #[serde(default)]
    failed: Vec<Target>,
    code: Option<String>,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    #[serde(rename = "type")]
    kind: String,
}

/// One row of the live context.
#[derive(Default)]
struct Entity {
    name: String,
    domain: String,
    state: String,
    area: String,
}

/// `None` when the answer does not read as the tool's, so the generic card
/// draws it instead. `tool` may carry the namespace of the API it came from.
pub(crate) fn render(
    tool: &str,
    arguments: Option<&Value>,
    output: &McpOutput,
    width: u16,
) -> Option<Rows> {
    if let [
        McpBlock::Resource {
            uri,
            mime_type,
            text: Some(text),
            bytes,
        },
    ] = output.blocks.as_slice()
    {
        let mut rows = Rows::default();
        rows.push(vec![resource_label(uri, mime_type, *bytes)]);
        rows.push(live_context_lines(&parse_live_context(text)?, width));
        return Some(rows);
    }
    let tool = tool.rsplit(NAMESPACE_SEPARATOR).next().unwrap_or(tool);
    let text = output.blocks.iter().find_map(|block| match block {
        McpBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })?;
    let mut rows = Rows::default();
    if tool == LIVE_CONTEXT_TOOL {
        rows.push(live_context_lines(&parse_live_context(text)?, width));
    } else {
        let response: IntentResponse = serde_json::from_str(text).ok()?;
        rows.push(intent_lines(&response, arguments, output.is_error));
    }
    Some(rows)
}

/// Where the call aimed, what Home Assistant said, then each target it
/// reached and each it did not.
fn intent_lines(
    response: &IntentResponse,
    arguments: Option<&Value>,
    is_error: bool,
) -> Vec<Line<'static>> {
    let theme = theme::current();
    let failed = is_error || response.response_type == ERROR_RESPONSE;
    let mut lines = Vec::new();
    if let Some(scope) = arguments.map(scope).filter(|scope| !scope.is_empty()) {
        lines.push(Line::from(Span::styled(scope, theme.tool_prefix)));
    }
    let said = response
        .speech
        .plain
        .as_ref()
        .map(|plain| plain.speech.trim())
        .filter(|speech| !speech.is_empty())
        .map_or_else(
            || response.response_type.replace(RESPONSE_WORD_SEPARATOR, " "),
            ToOwned::to_owned,
        );
    let mut head = vec![Span::styled(
        one_line(&said),
        if failed { theme.tool_error } else { theme.tool },
    )];
    if let Some(code) = &response.data.code {
        head.push(Span::styled(
            format!("{DETAIL_SEPARATOR}{}", one_line(code)),
            theme.tool_dim,
        ));
    }
    lines.push(Line::from(head));
    lines.extend(response.data.success.iter().map(|target| {
        target_line(
            target,
            SUCCEEDED_SIGIL,
            theme.tool_success,
            theme.tool,
            theme.tool_dim,
        )
    }));
    lines.extend(response.data.failed.iter().map(|target| {
        target_line(
            target,
            FAILED_SIGIL,
            theme.tool_error,
            theme.tool_error,
            theme.tool_error,
        )
    }));
    lines
}

/// `light · Kitchen`: the domains, device classes, area and floor the call
/// named, each a string or a list of them.
fn scope(arguments: &Value) -> String {
    SCOPE_KEYS
        .iter()
        .filter_map(|key| {
            let named = match arguments.get(key)? {
                Value::String(one) => vec![one.as_str()],
                Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => return None,
            };
            let named: Vec<&str> = named.into_iter().filter(|name| !name.is_empty()).collect();
            (!named.is_empty()).then(|| one_line(&named.join(LIST_SEPARATOR)))
        })
        .collect::<Vec<_>>()
        .join(DETAIL_SEPARATOR)
}

/// `✓ Kitchen Light · entity`.
fn target_line(
    target: &Target,
    sigil: &'static str,
    sigil_style: Style,
    name_style: Style,
    kind_style: Style,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(sigil, sigil_style),
        Span::styled(one_line(&target.name), name_style),
        Span::styled(
            format!("{DETAIL_SEPARATOR}{}", one_line(&target.kind)),
            kind_style,
        ),
    ])
}

/// The entities of `GetLiveContext`'s YAML listing, which arrives as the
/// `result` of a JSON object or bare. Only the fields the table shows are
/// read, so attributes and folded continuation lines are passed over. `None`
/// when no entity, or one without a name or a domain, comes out of it.
fn parse_live_context(text: &str) -> Option<Vec<Entity>> {
    let wrapped: Option<Value> = serde_json::from_str(text).ok();
    let listing = match &wrapped {
        Some(value) => value.get(LIVE_CONTEXT_RESULT_KEY)?.as_str()?,
        None => text,
    };
    let mut entities: Vec<Entity> = Vec::new();
    for line in listing.lines() {
        let field = if let Some(field) = line.strip_prefix(ENTITY_MARKER) {
            entities.push(Entity::default());
            field
        } else if let Some(field) = line
            .strip_prefix(FIELD_INDENT)
            .filter(|field| !field.starts_with(char::is_whitespace))
        {
            field
        } else {
            continue;
        };
        let (Some(entity), Some((key, value))) =
            (entities.last_mut(), field.split_once(KEY_SEPARATOR))
        else {
            continue;
        };
        let value = unquote(value.trim());
        let first = || {
            value
                .split(ALIAS_SEPARATOR)
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        match key {
            NAMES_KEY => entity.name = first(),
            DOMAIN_KEY => entity.domain = value.clone(),
            STATE_KEY => entity.state = value.clone(),
            AREAS_KEY => entity.area = first(),
            _ => {}
        }
    }
    let complete = entities
        .iter()
        .all(|entity| !entity.name.is_empty() && !entity.domain.is_empty());
    (complete && !entities.is_empty()).then_some(entities)
}

/// A YAML scalar without the quotes a dumper put around it.
fn unquote(value: &str) -> String {
    if let Some(inner) = value
        .strip_prefix(SINGLE_QUOTE)
        .and_then(|rest| rest.strip_suffix(SINGLE_QUOTE))
    {
        return inner.replace(ESCAPED_SINGLE_QUOTE, SINGLE_QUOTE_TEXT);
    }
    value
        .strip_prefix(DOUBLE_QUOTE)
        .and_then(|rest| rest.strip_suffix(DOUBLE_QUOTE))
        .unwrap_or(value)
        .to_owned()
}

/// The entities in aligned columns under their headings. No column is wider
/// than [`MAX_COLUMN_WIDTH`], and the name gives way first when the card is
/// narrow.
fn live_context_lines(entities: &[Entity], width: u16) -> Vec<Line<'static>> {
    let theme = theme::current();
    let cells: Vec<[String; 4]> = entities
        .iter()
        .map(|entity| {
            [&entity.name, &entity.domain, &entity.state, &entity.area].map(|cell| one_line(cell))
        })
        .collect();
    let mut widths = TABLE_HEADINGS.map(UnicodeWidthStr::width);
    for row in &cells {
        for (column, cell) in widths.iter_mut().zip(row) {
            *column = (*column).max(cell.width().min(MAX_COLUMN_WIDTH));
        }
    }
    let others = widths[1..].iter().sum::<usize>() + COLUMN_GAP.width() * (widths.len() - 1);
    widths[0] = widths[0]
        .min((width as usize).saturating_sub(others))
        .max(TABLE_HEADINGS[0].width());
    let mut lines = vec![table_row(
        &TABLE_HEADINGS.map(str::to_owned),
        &widths,
        [theme.tool_dim; 4],
    )];
    let styles = [theme.tool, theme.tool_dim, theme.accent, theme.tool_dim];
    lines.extend(cells.iter().map(|row| table_row(row, &widths, styles)));
    lines
}

fn table_row(cells: &[String; 4], widths: &[usize; 4], styles: [Style; 4]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, ((cell, width), style)) in cells.iter().zip(widths).zip(styles).enumerate() {
        let fitted = ellipsize_spans(vec![Span::styled(cell.clone(), style)], *width);
        let used: usize = fitted.iter().map(Span::width).sum();
        spans.extend(fitted);
        if index + 1 < cells.len() {
            spans.push(Span::raw(format!(
                "{}{COLUMN_GAP}",
                " ".repeat(width.saturating_sub(used))
            )));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use caudra_agent::McpServerKind;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const WIDTH: u16 = 80;
    const SPEECH: &str = "Turned on the lights";
    const KITCHEN_LIGHT: &str = "Kitchen Light";
    const PORCH_LAMP: &str = "Porch Lamp";
    const SNAPSHOT_URI: &str = "homeassistant://assist/context-snapshot";
    const PLAIN_MIME: &str = "text/plain";
    const READ_RESOURCE: &str = "read_resource";
    const LIVE_CONTEXT: &str = "Live Context: An overview of the areas and the devices in this smart home:\n\
        - names: Kitchen Light, Ceiling\n  domain: light\n  state: 'on'\n  areas: Kitchen, Cooking\n  attributes:\n    brightness: '255'\n\
        - names: Outside Temperature\n  domain: sensor\n  state: '12.5'\n  attributes:\n    unit_of_measurement: °C\n";

    fn output(text: &str) -> McpOutput {
        McpOutput {
            blocks: vec![McpBlock::Text { text: text.into() }],
            kind: Some(McpServerKind::HomeAssistant),
            ..McpOutput::default()
        }
    }

    fn intent(response_type: &str, success: &[&str], failed: &[&str]) -> String {
        let targets = |names: &[&str]| {
            names
                .iter()
                .map(|name| json!({"name": name, "type": "entity", "id": "light.x"}))
                .collect::<Vec<_>>()
        };
        json!({
            "speech": {"plain": {"speech": SPEECH, "extra_data": null}},
            "response_type": response_type,
            "data": {"success": targets(success), "failed": targets(failed)},
        })
        .to_string()
    }

    fn rendered(tool: &str, arguments: Option<&Value>, text: &str) -> Option<Rows> {
        render(tool, arguments, &output(text), WIDTH)
    }

    fn rows_text(rows: &Rows) -> Vec<String> {
        rows.lines.iter().map(ToString::to_string).collect()
    }

    #[test_case("HassTurnOn" ; "bare")]
    #[test_case("assist__HassTurnOn" ; "namespaced")]
    fn an_intent_says_what_it_did_and_where(tool: &str) {
        let arguments = json!({"domain": ["light"], "area": "Kitchen"});

        let rows = rendered(
            tool,
            Some(&arguments),
            &intent("action_done", &[KITCHEN_LIGHT], &[]),
        )
        .unwrap();

        assert_eq!(
            rows_text(&rows),
            [
                format!("light{DETAIL_SEPARATOR}Kitchen"),
                SPEECH.to_owned(),
                format!("{SUCCEEDED_SIGIL}{KITCHEN_LIGHT}{DETAIL_SEPARATOR}entity"),
            ]
        );
    }

    #[test]
    fn a_failed_target_is_in_the_error_colour() {
        let rows = rendered(
            "HassTurnOff",
            None,
            &intent("action_done", &[KITCHEN_LIGHT], &[PORCH_LAMP]),
        )
        .unwrap();

        let failed = rows.lines.last().unwrap();
        assert_eq!(
            failed.to_string(),
            format!("{FAILED_SIGIL}{PORCH_LAMP}{DETAIL_SEPARATOR}entity")
        );
        let error_style = theme::current().tool_error;
        assert!(failed.spans.iter().all(|span| span.style == error_style));
    }

    #[test]
    fn an_error_response_speaks_in_the_error_colour() {
        let rows = rendered("HassTurnOn", None, &intent(ERROR_RESPONSE, &[], &[])).unwrap();

        assert_eq!(rows.lines[0].spans[0].style, theme::current().tool_error);
    }

    #[test_case(LIVE_CONTEXT.to_owned() ; "bare")]
    #[test_case(json!({"result": LIVE_CONTEXT}).to_string() ; "wrapped")]
    fn the_live_context_is_a_table(text: String) {
        let rows = rendered("homeassistant__GetLiveContext", None, &text).unwrap();

        let rows = rows_text(&rows);
        let cells: Vec<Vec<&str>> = rows
            .iter()
            .map(|row| row.split_whitespace().collect())
            .collect();
        assert_eq!(cells[0], TABLE_HEADINGS);
        assert_eq!(cells[1], ["Kitchen", "Light", "light", "on", "Kitchen"]);
        assert_eq!(cells[2], ["Outside", "Temperature", "sensor", "12.5"]);
    }

    #[test]
    fn a_live_context_resource_is_a_table_under_its_label() {
        let output = McpOutput {
            blocks: vec![McpBlock::Resource {
                uri: SNAPSHOT_URI.into(),
                mime_type: Some(PLAIN_MIME.into()),
                text: Some(LIVE_CONTEXT.into()),
                bytes: None,
            }],
            ..output("")
        };

        let rows = rows_text(&render(READ_RESOURCE, None, &output, WIDTH).unwrap());

        assert_eq!(rows[0], format!("resource · {SNAPSHOT_URI} · {PLAIN_MIME}"));
        assert_eq!(
            rows[1].split_whitespace().collect::<Vec<_>>(),
            TABLE_HEADINGS
        );
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn a_resource_that_is_not_a_live_context_is_left_to_the_generic_card() {
        let output = McpOutput {
            blocks: vec![McpBlock::Resource {
                uri: SNAPSHOT_URI.into(),
                mime_type: Some(PLAIN_MIME.into()),
                text: Some("Live Context: nothing exposed".into()),
                bytes: None,
            }],
            ..output("")
        };

        assert!(render(READ_RESOURCE, None, &output, WIDTH).is_none());
    }

    #[test_case("GetLiveContext", "{\"error\": \"No entities\"}" ; "an_error_object")]
    #[test_case("GetLiveContext", "Live Context: nothing exposed" ; "no_entities")]
    #[test_case("GetLiveContext", "- domain: light\n  state: 'on'" ; "an_entity_without_a_name")]
    #[test_case("HassTurnOn", "not json" ; "intent_not_json")]
    #[test_case("GetDateTime", "{\"date\": \"2026-10-13\"}" ; "another_tool")]
    fn anything_else_is_left_to_the_generic_card(tool: &str, text: &str) {
        assert!(rendered(tool, None, text).is_none());
    }

    #[test_case("'on'", "on" ; "single_quoted")]
    #[test_case("'it''s'", "it's" ; "escaped_quote")]
    #[test_case("\"12\"", "12" ; "double_quoted")]
    #[test_case("plain", "plain" ; "plain")]
    fn yaml_quotes_are_stripped(value: &str, plain: &str) {
        assert_eq!(unquote(value), plain);
    }
}
