//! The card of a snippet's `history.search`, `history.read` or `history.sessions`: each hit under
//! its session and when, with the matched words marked; the item read, between the neighbours it
//! was read with; or each session listed. A row without the value it answered, as one saved
//! before rows kept it, is left to the generic card.

use caudra_agent::{HistoryHelper, McpBlock, McpOutput};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde::Deserialize;
use serde_json::Value;

use super::{Rows, escaped_lines, one_line};
use crate::components::code_view::{indented_rows, plain_body};
use crate::components::counted;
use crate::theme::{self, Theme};

const DETAIL_SEPARATOR: &str = " · ";
const SNIPPET_INDENT: &str = "  ";
const BEFORE_MARKER: &str = "↑ ";
const AFTER_MARKER: &str = "↓ ";
const TRUNCATED_SUFFIX: &str = "…";
const MATCH_OPEN: char = '«';
const MATCH_CLOSE: char = '»';
/// `2026-10-18T14:02`: an ISO time down to its minute.
const MINUTE_END: usize = 16;
const DATE_TIME_SEPARATOR: &str = "T";
const SPACE: &str = " ";
const UNTITLED: &str = "untitled";
const SUBAGENT_LABEL: &str = "subagent ";
const ITEM_NOUN: &str = "item";
const SCOPE_FIELD: &str = "scope";
/// The scope whose rows can come from any project, so each names its own.
const ALL_SCOPE: &str = "all";

#[derive(Default, Deserialize)]
#[serde(default)]
struct Found {
    hits: Vec<Hit>,
    note: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Hit {
    title: String,
    project: String,
    when: String,
    kind: String,
    tool: Option<String>,
    agent: Option<String>,
    snippet: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Entry {
    title: String,
    when: String,
    kind: String,
    tool: Option<String>,
    agent: Option<String>,
    text: String,
    before: Vec<Neighbour>,
    after: Vec<Neighbour>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Neighbour {
    when: String,
    kind: String,
    tool: Option<String>,
    preview: String,
    truncated: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Session {
    title: String,
    project: String,
    updated: String,
    items: usize,
}

pub(super) fn render(
    helper: HistoryHelper,
    arguments: Option<&Value>,
    output: &McpOutput,
    width: u16,
) -> Option<Rows> {
    let value = output.structured.as_ref()?;
    let theme = theme::current();
    let across = arguments
        .and_then(|arguments| arguments.get(SCOPE_FIELD))
        .and_then(Value::as_str)
        == Some(ALL_SCOPE);
    let body = match helper {
        HistoryHelper::Search => {
            search_rows(&Found::deserialize(value).ok()?, across, width, &theme)
        }
        HistoryHelper::Read => read_rows(&Entry::deserialize(value).ok()?, width, &theme),
        HistoryHelper::Sessions => {
            session_rows(&Vec::<Session>::deserialize(value).ok()?, across, &theme)
        }
    };
    let mut rows = Rows::default();
    if let Some(summary) = output.blocks.iter().find_map(|block| match block {
        McpBlock::Text { text } => Some(text),
        _ => None,
    }) {
        rows.push(vec![Line::from(Span::styled(
            one_line(summary),
            theme.tool_dim,
        ))]);
    }
    rows.append_apart(body);
    Some(rows)
}

/// Each hit's heading, its snippet indented under it, and a blank row between hits.
fn search_rows(found: &Found, across: bool, width: u16, theme: &Theme) -> Rows {
    let mut rows = Rows::default();
    for hit in &found.hits {
        let project = across.then_some(hit.project.as_str());
        let mut lines = vec![heading(
            &hit.title,
            [
                Some(minute(&hit.when)),
                Some(item_kind(&hit.kind, hit.tool.as_deref())),
                hit.agent.as_deref().map(subagent),
                project.map(one_line),
            ],
            theme,
        )];
        lines.extend(indented_rows(
            SNIPPET_INDENT,
            marked(&hit.snippet, theme),
            width,
        ));
        let mut hit_rows = Rows::default();
        hit_rows.push(lines);
        rows.append_apart(hit_rows);
    }
    if let Some(note) = found.note.as_deref().filter(|note| !note.is_empty()) {
        let mut note_rows = Rows::default();
        note_rows.push(vec![Line::from(Span::styled(
            one_line(note),
            theme.tool_dim,
        ))]);
        rows.append_apart(note_rows);
    }
    rows
}

/// The item's heading, the neighbours before it, its text as it was written, then the neighbours
/// after it.
fn read_rows(entry: &Entry, width: u16, theme: &Theme) -> Rows {
    let mut rows = Rows::default();
    let mut head = vec![heading(
        &entry.title,
        [
            Some(minute(&entry.when)),
            Some(item_kind(&entry.kind, entry.tool.as_deref())),
            entry.agent.as_deref().map(subagent),
        ],
        theme,
    )];
    head.extend(
        entry
            .before
            .iter()
            .map(|neighbour| neighbour_line(BEFORE_MARKER, neighbour, theme)),
    );
    rows.push(head);
    if !entry.text.trim().is_empty() {
        let mut text = Rows::default();
        text.push(plain_body(&escaped_lines(&entry.text), width).0);
        rows.append_apart(text);
    }
    let after: Vec<Line<'static>> = entry
        .after
        .iter()
        .map(|neighbour| neighbour_line(AFTER_MARKER, neighbour, theme))
        .collect();
    if !after.is_empty() {
        let mut after_rows = Rows::default();
        after_rows.push(after);
        rows.append_apart(after_rows);
    }
    rows
}

fn session_rows(sessions: &[Session], across: bool, theme: &Theme) -> Rows {
    let mut rows = Rows::default();
    rows.push(
        sessions
            .iter()
            .map(|session| {
                heading(
                    &session.title,
                    [
                        Some(minute(&session.updated)),
                        Some(counted(session.items, ITEM_NOUN)),
                        across.then(|| one_line(&session.project)),
                    ],
                    theme,
                )
            })
            .collect(),
    );
    rows
}

/// `Title · 2026-10-18 14:02 · assistant`: the title bright, what is known of it dimmed.
fn heading<const N: usize>(
    title: &str,
    details: [Option<String>; N],
    theme: &Theme,
) -> Line<'static> {
    let title = match one_line(title) {
        title if title.is_empty() => UNTITLED.to_owned(),
        title => title,
    };
    let mut spans = vec![Span::styled(title, theme.tool)];
    spans.extend(
        details
            .into_iter()
            .flatten()
            .filter(|detail| !detail.is_empty())
            .map(|detail| Span::styled(format!("{DETAIL_SEPARATOR}{detail}"), theme.tool_dim)),
    );
    Line::from(spans)
}

/// `↑ user · 2026-10-18 14:01 · the preview…`, all of it dimmed.
fn neighbour_line(marker: &'static str, neighbour: &Neighbour, theme: &Theme) -> Line<'static> {
    let suffix = if neighbour.truncated {
        TRUNCATED_SUFFIX
    } else {
        ""
    };
    Line::from(vec![
        Span::styled(marker, theme.tool_dim),
        Span::styled(
            [
                item_kind(&neighbour.kind, neighbour.tool.as_deref()),
                minute(&neighbour.when),
                format!("{}{suffix}", one_line(&neighbour.preview)),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(DETAIL_SEPARATOR),
            theme.tool_dim,
        ),
    ])
}

/// `tool_call shell`: the kind of item, and the tool it came from when it came from one.
fn item_kind(kind: &str, tool: Option<&str>) -> String {
    match tool.filter(|tool| !tool.is_empty()) {
        Some(tool) => format!("{} {}", one_line(kind), one_line(tool)),
        None => one_line(kind),
    }
}

fn subagent(agent: &str) -> String {
    format!("{SUBAGENT_LABEL}{}", one_line(agent))
}

fn minute(when: &str) -> String {
    when.get(..MINUTE_END)
        .unwrap_or(when)
        .replacen(DATE_TIME_SEPARATOR, SPACE, 1)
}

/// The snippet on one row, each `«match»` drawn without its marks in the accent colour.
fn marked(snippet: &str, theme: &Theme) -> Vec<Span<'static>> {
    let snippet = one_line(snippet);
    let mut spans = Vec::new();
    let mut push = |text: &str, style: Style| {
        if !text.is_empty() {
            spans.push(Span::styled(text.to_owned(), style));
        }
    };
    let mut rest = snippet.as_str();
    while let Some((before, opened)) = rest.split_once(MATCH_OPEN)
        && let Some((matched, after)) = opened.split_once(MATCH_CLOSE)
    {
        push(before, theme.tool);
        push(matched, theme.accent);
        rest = after;
    }
    push(rest, theme.tool);
    spans
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const WIDTH: u16 = 80;
    const SUMMARY: &str = "reranked 60 keyword matches, kept 60, showing 1";
    const TITLE: &str = "Code tools";
    const WHEN: &str = "2026-10-18T14:02:11Z";
    const SHOWN_WHEN: &str = "2026-10-18 14:02";
    const PROJECT: &str = "caudra";

    fn output(value: Value) -> McpOutput {
        McpOutput {
            blocks: vec![McpBlock::Text {
                text: SUMMARY.into(),
            }],
            structured: Some(value),
            ..McpOutput::default()
        }
    }

    fn lines(helper: HistoryHelper, arguments: Option<&Value>, value: Value) -> Vec<String> {
        render(helper, arguments, &output(value), WIDTH)
            .unwrap()
            .lines
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn search(project: &str) -> Value {
        json!({
            "hits": [{
                "ref": "s1:main:4", "session": "s1", "title": TITLE, "project": project,
                "when": WHEN, "kind": "assistant", "tool": null, "agent": null,
                "snippet": "we «test» the «code» tools", "score": 0.9,
            }],
            "ranking": "reranked", "judged_unrelated": 0, "more": false,
            "unindexed_sessions": 0, "note": null,
        })
    }

    #[test]
    fn a_search_lists_each_hit_under_its_session() {
        assert_eq!(
            lines(HistoryHelper::Search, None, search(PROJECT)),
            [
                SUMMARY.to_owned(),
                String::new(),
                format!("{TITLE}{DETAIL_SEPARATOR}{SHOWN_WHEN}{DETAIL_SEPARATOR}assistant"),
                format!("{SNIPPET_INDENT}we test the code tools"),
            ]
        );
    }

    #[test]
    fn a_match_is_drawn_in_the_accent_colour() {
        let theme = theme::current();
        let spans = marked("we «test» it", &theme);

        let accented: Vec<&str> = spans
            .iter()
            .filter(|span| span.style == theme.accent)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(accented, ["test"]);
    }

    #[test_case(json!({"scope": "all"}), true ; "all_projects")]
    #[test_case(json!({"scope": "project"}), false ; "this_project")]
    fn only_a_search_across_projects_names_each_hits_project(arguments: Value, named: bool) {
        let heading = &lines(HistoryHelper::Search, Some(&arguments), search(PROJECT))[2];

        assert_eq!(
            heading.ends_with(&format!("{DETAIL_SEPARATOR}{PROJECT}")),
            named
        );
    }

    #[test]
    fn a_read_draws_its_text_between_its_neighbours() {
        let value = json!({
            "title": TITLE, "when": WHEN, "kind": "tool_result", "tool": "shell",
            "text": "line one\nline two",
            "before": [{"when": WHEN, "kind": "tool_call", "tool": "shell", "preview": "ls", "truncated": false}],
            "after": [{"when": WHEN, "kind": "assistant", "tool": null, "preview": "done", "truncated": true}],
        });

        assert_eq!(
            lines(HistoryHelper::Read, None, value),
            [
                SUMMARY.to_owned(),
                String::new(),
                format!("{TITLE}{DETAIL_SEPARATOR}{SHOWN_WHEN}{DETAIL_SEPARATOR}tool_result shell"),
                format!(
                    "{BEFORE_MARKER}tool_call shell{DETAIL_SEPARATOR}{SHOWN_WHEN}{DETAIL_SEPARATOR}ls"
                ),
                String::new(),
                "line one".to_owned(),
                "line two".to_owned(),
                String::new(),
                format!(
                    "{AFTER_MARKER}assistant{DETAIL_SEPARATOR}{SHOWN_WHEN}{DETAIL_SEPARATOR}done{TRUNCATED_SUFFIX}"
                ),
            ]
        );
    }

    #[test]
    fn sessions_are_listed_one_a_row() {
        let value = json!([
            {"session": "s1", "title": TITLE, "project": PROJECT, "updated": WHEN, "items": 12, "subagent_items": 0},
            {"session": "s2", "title": "", "project": PROJECT, "updated": WHEN, "items": 1, "subagent_items": 0},
        ]);

        assert_eq!(
            lines(HistoryHelper::Sessions, None, value)[2..],
            [
                format!("{TITLE}{DETAIL_SEPARATOR}{SHOWN_WHEN}{DETAIL_SEPARATOR}12 items"),
                format!("{UNTITLED}{DETAIL_SEPARATOR}{SHOWN_WHEN}{DETAIL_SEPARATOR}1 item"),
            ]
        );
    }

    #[test_case(HistoryHelper::Search, json!([1, 2]) ; "a_search_not_an_object")]
    #[test_case(HistoryHelper::Sessions, json!({"hits": []}) ; "sessions_not_a_list")]
    fn anything_else_is_left_to_the_generic_card(helper: HistoryHelper, value: Value) {
        assert!(render(helper, None, &output(value), WIDTH).is_none());
    }

    #[test]
    fn a_row_saved_without_its_value_is_left_to_the_generic_card() {
        let saved = McpOutput {
            structured: None,
            ..output(Value::Null)
        };

        assert!(render(HistoryHelper::Search, None, &saved, WIDTH).is_none());
    }
}
