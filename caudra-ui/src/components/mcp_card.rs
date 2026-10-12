//! The transcript card of an MCP tool call: the script it ran, the rest of the
//! arguments it was given when the reader asked for them, then each block of
//! the answer drawn as what it is. JSON is indented and coloured, prose is
//! markdown, and media the model never read is a labelled row rather than a
//! wall of base64.

use caudra_agent::{McpBlock, McpOutput, ToolInput, pretty_json};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::components::code_view::{
    body_window, json_body, markdown_body, plain_body, truncation_line,
};
use crate::components::{escape_terminal_controls, format_bytes};
use crate::markdown::LinkMap;
use crate::theme;

pub(crate) const OUTPUT_DIVIDER: &str = "────────────";
const LABEL_SEPARATOR: &str = " · ";
const KEY_SUFFIX: &str = ":";
const LITERAL_INDENT: &str = "  ";
const LINE_BREAK: char = '\n';
const IMAGE_LABEL: &str = "image";
const AUDIO_LABEL: &str = "audio";
const RESOURCE_LABEL: &str = "resource";
const LINK_LABEL: &str = "link";
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

/// The card's lines, the links each of them carries, and whether the budget
/// left any of the answer out.
pub(crate) struct McpCard {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) links: LinkMap,
    pub(crate) truncated: bool,
}

/// Lines and their link rows, kept parallel as they grow.
#[derive(Default)]
struct Rows {
    lines: Vec<Line<'static>>,
    links: LinkMap,
}

impl Rows {
    fn push(&mut self, lines: Vec<Line<'static>>) {
        self.links.rows.extend(LinkMap::none_for(&lines).rows);
        self.lines.extend(lines);
    }

    fn push_linked(&mut self, lines: Vec<Line<'static>>, links: LinkMap) {
        self.links.rows.extend(links.rows);
        self.lines.extend(lines);
    }

    fn append(&mut self, other: Rows) {
        self.push_linked(other.lines, other.links);
    }

    fn truncate(&mut self, len: usize) {
        self.lines.truncate(len);
        self.links.rows.truncate(len);
    }
}

/// `budget` holds back only the answer: arguments are drawn when the reader
/// opened the card whole or while the call has nothing else to show. The
/// argument under `script_key` is left out, because the script above the card
/// already shows it, and `after_script` says that script is there.
pub(crate) fn render(
    arguments: Option<&Value>,
    script_key: Option<&str>,
    after_script: bool,
    output: Option<&McpOutput>,
    budget: usize,
    width: u16,
) -> McpCard {
    let mut card = Rows::default();
    let arguments = arguments.map(|arguments| without(arguments, script_key));
    if let Some(arguments) = arguments.filter(|arguments| !is_empty(arguments)) {
        if after_script {
            card.push(vec![Line::default()]);
        }
        push_arguments(&mut card, &arguments, width);
    }
    let mut answer = Rows::default();
    for block in output.map_or(&[][..], |output| output.blocks.as_slice()) {
        let drawn = block_rows(block, width);
        if drawn.lines.is_empty() {
            continue;
        }
        if !answer.lines.is_empty() {
            answer.push(vec![Line::default()]);
        }
        answer.append(drawn);
    }
    if (after_script || !card.lines.is_empty()) && !answer.lines.is_empty() {
        card.push(vec![Line::from(Span::styled(
            OUTPUT_DIVIDER,
            theme::current().tool_dim,
        ))]);
    }
    let (shown, hidden) = body_window(answer.lines.len(), budget);
    answer.truncate(shown);
    card.append(answer);
    if hidden > 0 {
        card.push(vec![truncation_line(hidden)]);
    }
    McpCard {
        lines: card.lines,
        links: card.links,
        truncated: hidden > 0,
    }
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
        let escaped = text
            .lines()
            .map(escape_terminal_controls)
            .collect::<Vec<_>>()
            .join("\n");
        let lines = plain_body(&escaped, literal_width)
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

fn block_rows(block: &McpBlock, width: u16) -> Rows {
    let mut rows = Rows::default();
    match block {
        McpBlock::Text { text } => push_text(&mut rows, text, width),
        McpBlock::Excerpt { text } => rows.push(json_body(text, width).0),
        McpBlock::Structured { value } => {
            if let Ok(pretty) = serde_json::to_string_pretty(value) {
                rows.push(json_body(&pretty, width).0);
            }
        }
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
                push_text(&mut rows, text, width);
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
fn push_text(rows: &mut Rows, text: &str, width: u16) {
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
        McpOutput { blocks }
    }

    fn rendered(card: &McpCard) -> Vec<String> {
        card.lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_json_block_is_drawn_indented_with_coloured_keys() {
        let card = render(
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
            Some(&arguments),
            None,
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
            Some(&arguments),
            None,
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
            None,
            None,
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
            Some(&arguments),
            Some("code"),
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
            Some(&arguments),
            Some("code"),
            true,
            Some(&output(vec![text("done")])),
            UNBOUNDED,
            WIDTH,
        );

        assert_eq!(rendered(&card), [OUTPUT_DIVIDER, "done"]);
    }
}
