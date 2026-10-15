//! The transcript card of an MCP tool call: the script it ran, the rest of the
//! arguments it was given when the reader asked for them, then each block of
//! the answer drawn as what it is. JSON is indented and coloured, prose is
//! markdown, and media the model never read is a labelled row rather than a
//! wall of base64. A server Caudra recognises draws its answer its own way,
//! and every card says what the server asked the user along the way.

use caudra_agent::types::Answerer;
use caudra_agent::{
    ElicitAction, McpBlock, McpInteraction, McpOutput, McpServerKind, ToolInput, mcp_tool_parts,
    pretty_json,
};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::components::code_view::{
    body_window, json_body, markdown_body, plain_body, truncation_line,
};
use crate::components::{escape_terminal_controls, format_bytes};
use crate::markdown::LinkMap;
use crate::theme;

pub(crate) mod decisions;
mod executor;
mod home_assistant;

pub(crate) const OUTPUT_DIVIDER: &str = "────────────";
const ASKED_LABEL: &str = "Asked: ";
const OUTCOME_ARROW: &str = " → ";
const ACCEPTED_LABEL: &str = "Accepted";
const DECLINED_LABEL: &str = "Declined";
const CANCELLED_LABEL: &str = "Cancelled";
const BY_USER_LABEL: &str = " by you";
const BY_AUTOPILOT_LABEL: &str = " by autopilot";
const URL_KEY: &str = "url";
pub(crate) const FIELD_INDENT: &str = "  ";
pub(crate) const FIELD_SEPARATOR: &str = ": ";
const SCHEMA_MISMATCH_NOTE: &str = "⚠ the answer does not match the tool's output schema: ";
const LABEL_SEPARATOR: &str = " · ";
const KEY_SUFFIX: &str = ":";
const LITERAL_INDENT: &str = "  ";
const LINE_BREAK: char = '\n';
const IMAGE_LABEL: &str = "image";
const AUDIO_LABEL: &str = "audio";
const RESOURCE_LABEL: &str = "resource";
const LINK_LABEL: &str = "link";
const STRUCTURED_LABEL: &str = "structuredContent";
/// Argument keys whose string is code the tool runs, in the order a script is
/// looked for.
const SCRIPT_KEYS: &[&str] = &[
    "code", "script", "source", "program", "sql", "command", "cmd",
];
const SQL_LANGUAGE: &str = "sql";
const SHELL_LANGUAGE: &str = "bash";
const PYTHON_LANGUAGE: &str = "python";
const JAVASCRIPT_LANGUAGE: &str = "js";
const PLAIN_LANGUAGE: &str = "text";
/// Keys that name their language outright.
const KEY_LANGUAGES: &[(&str, &str)] = &[
    ("sql", SQL_LANGUAGE),
    ("command", SHELL_LANGUAGE),
    ("cmd", SHELL_LANGUAGE),
];
const SQL_LEADS: &[&str] = &["select", "with", "insert", "update", "delete"];
const PYTHON_MARKERS: &[&str] = &["def ", "print(", "elif ", "self."];
const JAVASCRIPT_MARKERS: &[&str] = &["=>", "await ", "const ", "let ", "function", "console."];

/// The argument an MCP call ran, and the key it came under, so the rest of
/// the arguments can leave it out.
pub(crate) struct ScriptArgument {
    pub(crate) key: String,
    pub(crate) input: ToolInput,
}

/// The string the call was asked to run: the first under a key that names
/// code, or failing that the longest one spanning several lines.
pub(crate) fn script_argument(arguments: &Value) -> Option<ScriptArgument> {
    let fields = arguments.as_object()?;
    let strings = || {
        fields
            .iter()
            .filter_map(|(key, value)| Some((key, value.as_str()?)))
            .filter(|(_, text)| !text.trim().is_empty())
    };
    let (key, code) = SCRIPT_KEYS
        .iter()
        .find_map(|wanted| strings().find(|(key, _)| key.eq_ignore_ascii_case(wanted)))
        .or_else(|| {
            strings()
                .filter(|(_, text)| text.contains(LINE_BREAK))
                .max_by_key(|(_, text)| text.len())
        })?;
    Some(ScriptArgument {
        key: key.clone(),
        input: ToolInput::Code {
            language: script_language(key, code).to_owned(),
            code: code.to_owned(),
        },
    })
}

/// The language a key names, or else the one the text reads as.
fn script_language(key: &str, code: &str) -> &'static str {
    if let Some((_, language)) = KEY_LANGUAGES
        .iter()
        .find(|(named, _)| key.eq_ignore_ascii_case(named))
    {
        return language;
    }
    let lead = code
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if SQL_LEADS.contains(&lead.as_str()) {
        SQL_LANGUAGE
    } else if PYTHON_MARKERS.iter().any(|marker| code.contains(marker)) {
        PYTHON_LANGUAGE
    } else if JAVASCRIPT_MARKERS
        .iter()
        .any(|marker| code.contains(marker))
    {
        JAVASCRIPT_LANGUAGE
    } else {
        PLAIN_LANGUAGE
    }
}

/// The card's lines, the links each of them carries, whether the budget left
/// any of the answer out, and whether the answer reads differently as data.
pub(crate) struct McpCard {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) links: LinkMap,
    pub(crate) truncated: bool,
    pub(crate) has_raw_view: bool,
}

/// What the card knows of the call besides its answer.
#[derive(Default)]
pub(crate) struct McpCall<'a> {
    /// The tool's name as the transcript knows it, qualified by its server.
    pub(crate) tool: &'a str,
    pub(crate) arguments: Option<&'a Value>,
    /// Draw the arguments rather than only read them.
    pub(crate) show_arguments: bool,
    /// The argument the script above the card already shows.
    pub(crate) script_key: Option<&'a str>,
    /// Draw the answer as the data it came as, where that differs from its
    /// card.
    pub(crate) raw: bool,
}

impl McpCall<'_> {
    /// The tool's name as its server lists it.
    fn raw_tool(&self) -> &str {
        mcp_tool_parts(self.tool).map_or(self.tool, |(_, tool)| tool)
    }
}

/// Lines and their link rows, kept parallel as they grow.
#[derive(Default)]
pub(crate) struct Rows {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) links: LinkMap,
}

impl Rows {
    pub(crate) fn push(&mut self, lines: Vec<Line<'static>>) {
        self.links.rows.extend(LinkMap::none_for(&lines).rows);
        self.lines.extend(lines);
    }

    fn push_linked(&mut self, lines: Vec<Line<'static>>, links: LinkMap) {
        self.links.rows.extend(links.rows);
        self.lines.extend(lines);
    }

    pub(crate) fn append(&mut self, other: Rows) {
        self.push_linked(other.lines, other.links);
    }

    /// `other` after one blank row, when both have something to show.
    pub(crate) fn append_apart(&mut self, other: Rows) {
        if !self.lines.is_empty() && !other.lines.is_empty() {
            self.push(vec![Line::default()]);
        }
        self.append(other);
    }

    fn truncate(&mut self, len: usize) {
        self.lines.truncate(len);
        self.links.rows.truncate(len);
    }
}

/// `budget` holds back only the answer: arguments are drawn when the reader
/// opened the card whole or while the call has nothing else to show, and what
/// the server asked along the way always is. The argument under the call's
/// `script_key` is left out, because the script above the card already shows
/// it, and `after_script` says that script is there.
pub(crate) fn render(
    call: &McpCall<'_>,
    after_script: bool,
    output: Option<&McpOutput>,
    budget: usize,
    width: u16,
) -> McpCard {
    let mut card = Rows::default();
    let arguments = call
        .arguments
        .filter(|_| call.show_arguments)
        .map(|arguments| without(arguments, call.script_key));
    if let Some(arguments) = arguments.filter(|arguments| !is_empty(arguments)) {
        if after_script {
            card.push(vec![Line::default()]);
        }
        push_arguments(&mut card, &arguments, width);
    }
    let mut body = output.map_or_else(Rows::default, |output| {
        interaction_rows(&output.interactions)
    });
    let (mut answer, has_raw_view) = output.map_or_else(
        || (Rows::default(), false),
        |output| answer_rows(call, output, width, budget == usize::MAX),
    );
    let mismatch = output.and_then(|output| output.schema_mismatch.as_deref());
    if (after_script || !card.lines.is_empty())
        && (!body.lines.is_empty() || !answer.lines.is_empty() || mismatch.is_some())
    {
        card.push(vec![Line::from(Span::styled(
            OUTPUT_DIVIDER,
            theme::current().tool_dim,
        ))]);
    }
    let (shown, hidden) = body_window(answer.lines.len(), budget);
    answer.truncate(shown);
    body.append_apart(answer);
    card.append(body);
    if hidden > 0 {
        card.push(vec![truncation_line(hidden)]);
    }
    if let Some(reason) = mismatch {
        card.push(vec![mismatch_line(reason)]);
    }
    McpCard {
        lines: card.lines,
        links: card.links,
        truncated: hidden > 0,
        has_raw_view,
    }
}

/// A recognised server's own card when its answer reads as that server's,
/// and every block drawn as what it is otherwise, unless the call asks for
/// the raw view. That view exists where it shows something the card does
/// not: what a recognised server's card read its rows out of, or a
/// `structuredContent` the generic card leaves to the text beside it.
fn answer_rows(call: &McpCall<'_>, output: &McpOutput, width: u16, whole: bool) -> (Rows, bool) {
    let recognised = match (output.helper, output.kind) {
        (Some(helper), _) => decisions::render(helper, call.arguments, output, width),
        (None, Some(McpServerKind::Executor)) => {
            executor::render(call.raw_tool(), output, width, whole)
        }
        (None, Some(McpServerKind::HomeAssistant)) => {
            home_assistant::render(call.raw_tool(), call.arguments, output, width)
        }
        (None, None) => None,
    };
    let has_raw_view = recognised.is_some() || unspelled_structured(output).is_some();
    let rows = match recognised {
        _ if call.raw && has_raw_view => raw_rows(output, width),
        Some(card) => card,
        None => blocks_rows(&output.blocks, width),
    };
    (rows, has_raw_view)
}

pub(crate) fn blocks_rows<'a>(blocks: impl IntoIterator<Item = &'a McpBlock>, width: u16) -> Rows {
    let mut rows = Rows::default();
    for block in blocks {
        rows.append_apart(block_rows(block, width, false));
    }
    rows
}

/// The answer as the data it came as: a JSON document indented and coloured
/// the way the generic card draws one, any other text exactly as the server
/// wrote it, and a `structuredContent` no text block spells out under its
/// name.
fn raw_rows(output: &McpOutput, width: u16) -> Rows {
    let mut rows = Rows::default();
    for block in &output.blocks {
        rows.append_apart(block_rows(block, width, true));
    }
    if let Some(value) = unspelled_structured(output) {
        let mut structured = Rows::default();
        structured.push(vec![Line::from(Span::styled(
            format!("{STRUCTURED_LABEL}{KEY_SUFFIX}"),
            theme::current().accent,
        ))]);
        push_value(&mut structured, value, width);
        rows.append_apart(structured);
    }
    rows
}

/// The `structuredContent` kept beside the text, unless a text block is the
/// same document, as Executor's is.
fn unspelled_structured(output: &McpOutput) -> Option<&Value> {
    output.structured.as_ref().filter(|value| {
        !output.blocks.iter().any(|block| match block {
            McpBlock::Text { text } => {
                serde_json::from_str::<Value>(text).is_ok_and(|parsed| &parsed == *value)
            }
            _ => false,
        })
    })
}

fn push_value(rows: &mut Rows, value: &Value, width: u16) {
    if let Ok(pretty) = serde_json::to_string_pretty(value) {
        rows.push(json_body(&pretty, width).0);
    }
}

/// `Asked: message → Accepted by you`, then each field answered.
fn interaction_rows(interactions: &[McpInteraction]) -> Rows {
    let theme = theme::current();
    let mut rows = Rows::default();
    for interaction in interactions {
        let (outcome, style) = match interaction.action {
            ElicitAction::Accept => (ACCEPTED_LABEL, theme.tool_success),
            ElicitAction::Decline => (DECLINED_LABEL, theme.tool_warning),
            ElicitAction::Cancel => (CANCELLED_LABEL, theme.tool_dim),
        };
        let by = match interaction.by {
            Answerer::User => BY_USER_LABEL,
            Answerer::Autopilot => BY_AUTOPILOT_LABEL,
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(ASKED_LABEL, theme.tool_dim),
            Span::styled(one_line(&interaction.message), theme.tool),
            Span::styled(OUTCOME_ARROW, theme.tool_dim),
            Span::styled(outcome, style),
            Span::styled(by, theme.tool_dim),
        ])];
        let url = interaction.url.as_deref().map(|url| (URL_KEY, url));
        let answers = interaction
            .answers
            .iter()
            .map(|(label, value)| (label.as_str(), value.as_str()));
        lines.extend(
            url.into_iter()
                .chain(answers)
                .map(|(label, value)| field_line(label, Span::styled(one_line(value), theme.tool))),
        );
        rows.push(lines);
    }
    rows
}

/// `  label: value`, the label dimmed.
pub(crate) fn field_line(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{FIELD_INDENT}{}{FIELD_SEPARATOR}", one_line(label)),
            theme::current().tool_dim,
        ),
        value,
    ])
}

fn mismatch_line(reason: &str) -> Line<'static> {
    let theme = theme::current();
    Line::from(vec![
        Span::styled(SCHEMA_MISMATCH_NOTE, theme.tool_warning),
        Span::styled(one_line(reason), theme.tool_dim),
    ])
}

/// Server text for a single row: its whitespace runs collapsed, and nothing
/// left that a terminal would act on.
pub(crate) fn one_line(text: &str) -> String {
    escape_terminal_controls(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn without(arguments: &Value, key: Option<&str>) -> Value {
    match (arguments, key) {
        (Value::Object(fields), Some(key)) => {
            let mut rest = fields.clone();
            rest.shift_remove(key);
            Value::Object(rest)
        }
        _ => arguments.clone(),
    }
}

fn is_empty(arguments: &Value) -> bool {
    match arguments {
        Value::Null => true,
        Value::Object(fields) => fields.is_empty(),
        _ => false,
    }
}

/// The arguments as indented JSON, except a string spanning several lines,
/// which is code or prose the model wrote and reads as itself under its key
/// rather than as one escaped line.
fn push_arguments(rows: &mut Rows, arguments: &Value, width: u16) {
    let mut literals = Vec::new();
    let structured = match arguments {
        Value::Object(fields) => {
            let mut rest = fields.clone();
            rest.retain(|key, value| match value.as_str() {
                Some(text) if text.contains(LINE_BREAK) => {
                    literals.push((key.clone(), text.to_owned()));
                    false
                }
                _ => true,
            });
            (!rest.is_empty()).then_some(Value::Object(rest))
        }
        other => Some(other.clone()),
    };
    if let Some(pretty) = structured.and_then(|value| serde_json::to_string_pretty(&value).ok()) {
        rows.push(json_body(&pretty, width).0);
    }
    let theme = theme::current();
    let literal_width = width.saturating_sub(LITERAL_INDENT.len() as u16).max(1);
    for (key, text) in literals {
        rows.push(vec![Line::from(Span::styled(
            format!("{}{KEY_SUFFIX}", escape_terminal_controls(&key)),
            theme.accent,
        ))]);
        let lines = plain_body(&escaped_lines(&text), literal_width)
            .0
            .into_iter()
            .map(|mut line| {
                line.spans.insert(0, Span::raw(LITERAL_INDENT));
                line
            })
            .collect();
        rows.push(lines);
    }
}

/// One block drawn as what it is. `raw` keeps text that is not a JSON
/// document as the server wrote it rather than rendering it as markdown.
fn block_rows(block: &McpBlock, width: u16, raw: bool) -> Rows {
    let push = if raw { push_raw_text } else { push_text };
    let mut rows = Rows::default();
    match block {
        McpBlock::Text { text } => push(&mut rows, text, width),
        McpBlock::Excerpt { text } => rows.push(json_body(text, width).0),
        McpBlock::Structured { value } => push_value(&mut rows, value, width),
        McpBlock::Image { mime_type, bytes } => rows.push(vec![label_row(
            IMAGE_LABEL,
            [mime_type.clone(), Some(format_bytes(*bytes as u64))],
        )]),
        McpBlock::Audio { mime_type, bytes } => rows.push(vec![label_row(
            AUDIO_LABEL,
            [mime_type.clone(), Some(format_bytes(*bytes as u64))],
        )]),
        McpBlock::Resource {
            uri,
            mime_type,
            text,
            bytes,
        } => {
            rows.push(vec![label_row(
                RESOURCE_LABEL,
                [
                    Some(uri.clone()),
                    mime_type.clone(),
                    bytes.map(|bytes| format_bytes(bytes as u64)),
                ],
            )]);
            if let Some(text) = text {
                push(&mut rows, text, width);
            }
        }
        McpBlock::ResourceLink {
            uri,
            name,
            mime_type,
        } => rows.push(vec![label_row(
            LINK_LABEL,
            [name.clone(), Some(uri.clone()), mime_type.clone()],
        )]),
    }
    rows
}

/// JSON when the whole block is a document, markdown otherwise.
pub(crate) fn push_text(rows: &mut Rows, text: &str, width: u16) {
    if text.trim().is_empty() {
        return;
    }
    match pretty_json(text) {
        Some(pretty) => rows.push(json_body(&pretty, width).0),
        None => {
            let (lines, _, links) = markdown_body(text, width);
            rows.push_linked(lines, links);
        }
    }
}

/// JSON when the whole block is a document, the text as it came otherwise.
fn push_raw_text(rows: &mut Rows, text: &str, width: u16) {
    if text.trim().is_empty() {
        return;
    }
    match pretty_json(text) {
        Some(pretty) => rows.push(json_body(&pretty, width).0),
        None => rows.push(plain_body(&escaped_lines(text), width).0),
    }
}

/// Each line made safe for the terminal on its own, since escaping the whole
/// text would turn its line breaks into visible escapes too.
fn escaped_lines(text: &str) -> String {
    text.lines()
        .map(escape_terminal_controls)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `image · image/png · 12.0 KB`: what the block is, then what is known of it.
fn label_row<const N: usize>(kind: &str, details: [Option<String>; N]) -> Line<'static> {
    let theme = theme::current();
    let mut spans = vec![Span::styled(kind.to_owned(), theme.tool_prefix)];
    for detail in details.into_iter().flatten().filter(|d| !d.is_empty()) {
        spans.push(Span::styled(
            format!("{LABEL_SEPARATOR}{}", escape_terminal_controls(&detail)),
            theme.tool_dim,
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const WIDTH: u16 = 80;
    const UNBOUNDED: usize = usize::MAX;
    const JSON_TEXT: &str = "{\"name\":\"value\",\"items\":[1,2]}";
    const KEY_NOT_ACCENTED: &str = "a JSON key must take the theme's accent";

    fn text(text: &str) -> McpBlock {
        McpBlock::Text { text: text.into() }
    }

    fn output(blocks: Vec<McpBlock>) -> McpOutput {
        McpOutput {
            blocks,
            ..McpOutput::default()
        }
    }

    fn rendered(card: &McpCard) -> Vec<String> {
        card.lines.iter().map(ToString::to_string).collect()
    }

    fn call<'a>(arguments: Option<&'a Value>, script_key: Option<&'a str>) -> McpCall<'a> {
        McpCall {
            arguments,
            show_arguments: true,
            script_key,
            ..McpCall::default()
        }
    }

    #[test]
    fn a_json_block_is_drawn_indented_with_coloured_keys() {
        let card = render(
            &McpCall::default(),
            false,
            Some(&output(vec![text(JSON_TEXT)])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(rendered(&card)[..2], ["{", "  \"name\": \"value\","]);
        let key = card.lines[1]
            .spans
            .iter()
            .find(|span| span.content == "\"name\"")
            .expect(KEY_NOT_ACCENTED);
        assert_eq!(key.style, theme::current().accent, "{KEY_NOT_ACCENTED}");
        assert_eq!(card.links.rows.len(), card.lines.len());
    }

    #[test]
    fn a_prose_block_is_drawn_as_markdown() {
        let card = render(
            &McpCall::default(),
            false,
            Some(&output(vec![text("# Heading\n\nbody **bold**")])),
            UNBOUNDED,
            WIDTH,
        );

        let lines = rendered(&card);
        assert!(lines.iter().all(|line| !line.contains('#')), "{lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("body bold")),
            "{lines:?}"
        );
    }

    #[test_case(McpBlock::Image { mime_type: Some("image/png".into()), bytes: 2048 }, "image · image/png · 2.0 KB" ; "image")]
    #[test_case(McpBlock::Audio { mime_type: None, bytes: 10 }, "audio · 10 B" ; "audio")]
    #[test_case(McpBlock::ResourceLink { uri: "file:///a".into(), name: Some("a".into()), mime_type: None }, "link · a · file:///a" ; "link")]
    #[test_case(McpBlock::Resource { uri: "file:///b".into(), mime_type: None, text: None, bytes: Some(1) }, "resource · file:///b · 1 B" ; "blob")]
    fn media_is_a_labelled_row(block: McpBlock, label: &str) {
        let card = render(
            &McpCall::default(),
            false,
            Some(&output(vec![block])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(rendered(&card), [label]);
    }

    #[test]
    fn blocks_are_separated_by_one_blank_row() {
        let card = render(
            &McpCall::default(),
            false,
            Some(&output(vec![text("[1]"), text(""), text("[2]")])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(rendered(&card), ["[", "  1", "]", "", "[", "  2", "]"]);
    }

    #[test]
    fn the_budget_keeps_the_head_and_says_what_it_left_out() {
        let long = format!(
            "[{}]",
            (0..50).map(|n| n.to_string()).collect::<Vec<_>>().join(",")
        );
        let card = render(
            &McpCall::default(),
            false,
            Some(&output(vec![text(&long)])),
            5,
            WIDTH,
        );

        assert!(card.truncated);
        assert_eq!(card.lines.len(), 5);
        assert_eq!(rendered(&card)[0], "[");
    }

    #[test]
    fn arguments_come_first_with_long_strings_drawn_literally() {
        let arguments = serde_json::json!({"limit": 3, "code": "let a = 1;\nlet b = \"x\";"});
        let card = render(
            &call(Some(&arguments), None),
            false,
            Some(&output(vec![text("done")])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(
            rendered(&card),
            [
                "{",
                "  \"limit\": 3",
                "}",
                "code:",
                "  let a = 1;",
                "  let b = \"x\";",
                OUTPUT_DIVIDER,
                "done",
            ]
        );
    }

    #[test_case(serde_json::json!({}) ; "empty_object")]
    #[test_case(Value::Null ; "null")]
    fn empty_arguments_draw_nothing(arguments: Value) {
        let card = render(
            &call(Some(&arguments), None),
            false,
            Some(&output(vec![text("done")])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(rendered(&card), ["done"]);
    }

    #[test]
    fn an_excerpt_of_cut_json_paints_every_line() {
        let excerpt = "[\n  {\n    \"a\": \"unterminated\n\n[Tool output truncated.]";
        let card = render(
            &McpCall::default(),
            false,
            Some(&output(vec![McpBlock::Excerpt {
                text: excerpt.into(),
            }])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(rendered(&card).join("\n"), excerpt);
    }

    const JS_CODE: &str = "let p = await tools.search({limit: 10}); return p;";

    fn script(arguments: Value) -> Option<(String, String, String)> {
        script_argument(&arguments).map(|script| {
            let ToolInput::Code { language, code } = script.input else {
                unreachable!("a script argument is code");
            };
            (script.key, language, code)
        })
    }

    #[test_case(serde_json::json!({"limit": 3, "code": JS_CODE}), Some("code") ; "code_key")]
    #[test_case(serde_json::json!({"note": "a\nb", "script": "x"}), Some("script") ; "known_key_beats_multiline")]
    #[test_case(serde_json::json!({"sql": "select 1"}), Some("sql") ; "sql_key")]
    #[test_case(serde_json::json!({"command": "ls"}), Some("command") ; "command_key")]
    #[test_case(serde_json::json!({"a": "x\ny", "b": "longer\ntext\nhere"}), Some("b") ; "longest_multiline")]
    #[test_case(serde_json::json!({"query": "find docs"}), None ; "short_query")]
    #[test_case(serde_json::json!({"code": "  "}), None ; "blank_code")]
    #[test_case(serde_json::json!(["code"]), None ; "not_an_object")]
    fn the_script_is_code_under_its_key_or_the_longest_multiline_string(
        arguments: Value,
        key: Option<&str>,
    ) {
        assert_eq!(script(arguments).map(|(key, _, _)| key).as_deref(), key);
    }

    #[test_case("code", JS_CODE, JAVASCRIPT_LANGUAGE ; "javascript")]
    #[test_case("code", "def f():\n    return 1", PYTHON_LANGUAGE ; "python")]
    #[test_case("code", "SELECT * FROM t", SQL_LANGUAGE ; "sql_by_text")]
    #[test_case("sql", "x", SQL_LANGUAGE ; "sql_by_key")]
    #[test_case("cmd", "ls -la", SHELL_LANGUAGE ; "shell_by_key")]
    #[test_case("code", "plain words", PLAIN_LANGUAGE ; "plain")]
    fn the_script_language_comes_from_its_key_or_its_text(key: &str, code: &str, language: &str) {
        assert_eq!(script_language(key, code), language);
    }

    #[test]
    fn the_script_key_is_left_out_of_the_other_arguments() {
        let arguments = serde_json::json!({"code": JS_CODE, "limit": 3});
        let card = render(
            &call(Some(&arguments), Some("code")),
            true,
            Some(&output(vec![text("done")])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(
            rendered(&card),
            ["", "{", "  \"limit\": 3", "}", OUTPUT_DIVIDER, "done"]
        );
    }

    #[test]
    fn a_script_alone_still_divides_it_from_the_answer() {
        let arguments = serde_json::json!({"code": JS_CODE});
        let card = render(
            &call(Some(&arguments), Some("code")),
            true,
            Some(&output(vec![text("done")])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(rendered(&card), [OUTPUT_DIVIDER, "done"]);
    }

    const ASKED: &str = "Deploy to production?";
    const ANSWER_LABEL: &str = "environment";
    const ANSWER_VALUE: &str = "prod";
    const MISMATCH: &str = "missing property `id`";
    const EXECUTOR_TOOL: &str = "executor.execute";
    const COMPLETED_RUN: &str = "{\"status\":\"completed\",\"result\":\"done\",\"logs\":[]}";

    fn interaction(action: ElicitAction, by: Answerer) -> McpInteraction {
        McpInteraction {
            message: ASKED.into(),
            url: None,
            action,
            by,
            answers: vec![(ANSWER_LABEL.into(), ANSWER_VALUE.into())],
        }
    }

    fn recognised(kind: McpServerKind, block: &str) -> McpOutput {
        McpOutput {
            kind: Some(kind),
            ..output(vec![text(block)])
        }
    }

    fn with_structured(structured: &str) -> McpOutput {
        McpOutput {
            structured: serde_json::from_str(structured).ok(),
            ..recognised(McpServerKind::Executor, COMPLETED_RUN)
        }
    }

    fn tool_card(tool: &str, output: &McpOutput) -> Vec<String> {
        rendered(&viewed(tool, output, false))
    }

    fn viewed(tool: &str, output: &McpOutput, raw: bool) -> McpCard {
        let call = McpCall {
            tool,
            raw,
            ..McpCall::default()
        };
        render(&call, false, Some(output), UNBOUNDED, WIDTH)
    }

    #[test_case(ElicitAction::Accept, Answerer::User, "Accepted by you" ; "accepted_by_you")]
    #[test_case(ElicitAction::Decline, Answerer::Autopilot, "Declined by autopilot" ; "declined_by_autopilot")]
    #[test_case(ElicitAction::Cancel, Answerer::User, "Cancelled by you" ; "cancelled")]
    fn what_the_server_asked_leads_the_answer(action: ElicitAction, by: Answerer, outcome: &str) {
        let asked = McpOutput {
            interactions: vec![interaction(action, by)],
            ..output(vec![text("done")])
        };

        let card = render(&McpCall::default(), false, Some(&asked), UNBOUNDED, WIDTH);

        assert_eq!(
            rendered(&card),
            [
                format!("{ASKED_LABEL}{ASKED}{OUTCOME_ARROW}{outcome}"),
                format!("{FIELD_INDENT}{ANSWER_LABEL}{FIELD_SEPARATOR}{ANSWER_VALUE}"),
                String::new(),
                "done".to_owned(),
            ]
        );
    }

    #[test]
    fn a_schema_mismatch_is_noted_under_the_answer() {
        let mismatched = McpOutput {
            schema_mismatch: Some(MISMATCH.into()),
            ..output(vec![text("done")])
        };

        let card = render(
            &McpCall::default(),
            false,
            Some(&mismatched),
            UNBOUNDED,
            WIDTH,
        );

        let note = card.lines.last().unwrap();
        assert_eq!(
            note.to_string(),
            format!("{SCHEMA_MISMATCH_NOTE}{MISMATCH}")
        );
        assert_eq!(note.spans[0].style, theme::current().tool_warning);
    }

    #[test]
    fn a_recognised_answer_draws_its_servers_card() {
        let rows = tool_card(EXECUTOR_TOOL, &with_structured(COMPLETED_RUN));

        assert_eq!(rows, ["completed", "done"]);
    }

    #[test_case("executor.integrations", Some(McpServerKind::Executor), COMPLETED_RUN ; "unknown_tool")]
    #[test_case(EXECUTOR_TOOL, Some(McpServerKind::Executor), "{\"status\":7}" ; "unparsed_answer")]
    #[test_case(EXECUTOR_TOOL, None, COMPLETED_RUN ; "unrecognised_server")]
    fn anything_unrecognised_draws_the_generic_card(
        tool: &str,
        kind: Option<McpServerKind>,
        structured: &str,
    ) {
        let answer = McpOutput {
            kind,
            ..with_structured(structured)
        };

        let rows = tool_card(tool, &answer);

        let generic = tool_card(tool, &output(answer.blocks.clone()));
        assert_eq!(rows, generic);
        assert_eq!(rows[0], "{");
    }

    #[test]
    fn a_home_assistant_intent_draws_its_card_under_a_qualified_name() {
        let response = serde_json::json!({
            "speech": {"plain": {"speech": "Done"}},
            "response_type": "action_done",
            "data": {"success": [], "failed": []},
        });
        let answer = recognised(McpServerKind::HomeAssistant, &response.to_string());

        assert_eq!(tool_card("home-assistant.HassTurnOn", &answer), ["Done"]);
    }

    const MARKDOWN_TEXT: &str = "**done**";
    const UNSPELLED: &str = "{\"id\":7}";
    const GENERIC_TOOL: &str = "search.find";

    fn generic(structured: Option<&str>) -> McpOutput {
        McpOutput {
            structured: structured.and_then(|value| serde_json::from_str(value).ok()),
            ..output(vec![text(MARKDOWN_TEXT)])
        }
    }

    #[test_case(EXECUTOR_TOOL, with_structured(COMPLETED_RUN), true ; "recognised_card")]
    #[test_case("executor.integrations", with_structured(COMPLETED_RUN), false ; "recognised_server_unknown_tool")]
    #[test_case(GENERIC_TOOL, generic(None), false ; "text_only")]
    #[test_case(GENERIC_TOOL, generic(Some(UNSPELLED)), true ; "unspelled_structured_content")]
    #[test_case(GENERIC_TOOL, output(vec![text(UNSPELLED)]), false ; "json_text_only")]
    fn a_raw_view_exists_only_where_it_shows_more(tool: &str, answer: McpOutput, expected: bool) {
        assert_eq!(viewed(tool, &answer, false).has_raw_view, expected);
        assert_eq!(viewed(tool, &answer, true).has_raw_view, expected);
    }

    #[test]
    fn a_recognised_answer_reads_raw_as_its_json() {
        let rows = rendered(&viewed(
            EXECUTOR_TOOL,
            &with_structured(COMPLETED_RUN),
            true,
        ));

        assert_eq!(rows[..2], ["{", "  \"status\": \"completed\","]);
    }

    #[test]
    fn the_raw_view_keeps_text_as_written_and_spells_out_structured_content() {
        let rows = rendered(&viewed(GENERIC_TOOL, &generic(Some(UNSPELLED)), true));

        assert_eq!(
            rows,
            [
                MARKDOWN_TEXT,
                "",
                format!("{STRUCTURED_LABEL}{KEY_SUFFIX}").as_str(),
                "{",
                "  \"id\": 7",
                "}",
            ]
        );
    }

    #[test]
    fn a_raw_request_without_a_raw_view_draws_the_card() {
        let answer = generic(None);

        assert_eq!(
            rendered(&viewed(GENERIC_TOOL, &answer, true)),
            tool_card(GENERIC_TOOL, &answer)
        );
    }
}
