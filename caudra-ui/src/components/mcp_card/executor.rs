//! The answer of an Executor run, read from the `structuredContent` Executor
//! sends beside its text: how the run ended, what it returned, what it
//! logged, and what a paused run waits for. A paused run's link is shown as
//! text and never opened: approving it is the user's call, made elsewhere.

use caudra_agent::{McpBlock, McpOutput};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde::Deserialize;
use serde_json::Value;

use super::{Rows, blocks_rows, field_line, one_line, push_text};
use crate::components::code_view::json_body;
use crate::components::escape_terminal_controls;
use crate::theme;

const EXECUTE_TOOL: &str = "execute";
const RESUME_TOOL: &str = "resume";
const SEARCH_TOOL_PREFIX: &str = "search_";
/// Log lines a resting card shows before it folds the rest into a count.
const MAX_LOG_LINES: usize = 8;
const COMPLETED_LABEL: &str = "completed";
const ERROR_LABEL: &str = "error";
const WAITING_LABEL: &str = "waiting for approval";
const DETAIL_SEPARATOR: &str = " · ";
const EMITTED_SUFFIX: &str = " emitted";
const EXPIRES_PREFIX: &str = "expires ";
const LOGS_LABEL: &str = "logs";
const LOG_INDENT: &str = "  ";
const MORE_LOGS_PREFIX: &str = "+";
const MORE_LOGS_SUFFIX: &str = " more";
const URL_LABEL: &str = "url";
const ERROR_CODE_KEY: &str = "code";
const ERROR_MESSAGE_KEY: &str = "message";
const ERROR_CODE_SEPARATOR: &str = ": ";
const ARGS_OPEN: &str = "(";
const ARGS_CLOSE: &str = ")";
/// Characters of a paused call's arguments its one-line summary keeps.
const MAX_ARGS_SUMMARY_CHARS: usize = 80;
const ELLIPSIS: char = '…';

/// `structuredContent` as `formatExecuteResult` and `formatPausedExecution`
/// write it.
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Run {
    Completed {
        #[serde(default)]
        result: Value,
        #[serde(default, rename = "toolName")]
        tool_name: Option<String>,
        #[serde(default)]
        emitted: usize,
        #[serde(default)]
        logs: Vec<String>,
    },
    Error {
        error: Value,
        #[serde(default)]
        emitted: usize,
        #[serde(default)]
        logs: Vec<String>,
    },
    WaitingForInteraction {
        #[serde(default, rename = "expiresAt")]
        expires_at: Option<String>,
        interaction: Interaction,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum InteractionKind {
    Form,
    Url,
}

#[derive(Deserialize)]
struct Interaction {
    kind: InteractionKind,
    message: String,
    #[serde(default)]
    address: String,
    #[serde(default)]
    args: Value,
    url: Option<String>,
}

/// `None` when the tool is not one that runs code, or its answer does not
/// read as a run, so the generic card draws it instead. `whole` unfolds the
/// logs.
pub(crate) fn render(tool: &str, output: &McpOutput, width: u16, whole: bool) -> Option<Rows> {
    if !runs_code(tool) {
        return None;
    }
    let run = Run::deserialize(output.structured_content()?).ok()?;
    let theme = theme::current();
    let mut rows = Rows::default();
    match run {
        Run::Completed {
            result,
            tool_name,
            emitted,
            logs,
        } => {
            let style = match output.is_error {
                true => theme.tool_error,
                false => theme.tool_success,
            };
            let mut status = vec![Span::styled(COMPLETED_LABEL, style)];
            status.extend(tool_name.as_deref().map(detail));
            status.extend(emitted_detail(emitted));
            rows.push(vec![Line::from(status)]);
            rows.append(result_rows(&result, width));
            let envelope = !result.is_null() || !logs.is_empty();
            push_emitted(&mut rows, output, emitted, envelope, width);
            rows.append_apart(log_rows(&logs, whole));
        }
        Run::Error {
            error,
            emitted,
            logs,
        } => {
            let mut status = vec![Span::styled(ERROR_LABEL, theme.tool_error)];
            status.extend(emitted_detail(emitted));
            rows.push(vec![Line::from(status)]);
            rows.append(error_rows(&error, width));
            push_emitted(&mut rows, output, emitted, true, width);
            rows.append_apart(log_rows(&logs, whole));
        }
        Run::WaitingForInteraction {
            expires_at,
            interaction,
        } => {
            let mut status = vec![Span::styled(WAITING_LABEL, theme.tool_warning)];
            status.extend(
                expires_at
                    .as_deref()
                    .map(|expiry| detail(&format!("{EXPIRES_PREFIX}{expiry}"))),
            );
            rows.push(vec![Line::from(status)]);
            rows.push(waiting_lines(&interaction));
        }
    }
    Some(rows)
}

fn runs_code(tool: &str) -> bool {
    tool == EXECUTE_TOOL || tool == RESUME_TOOL || tool.starts_with(SEARCH_TOOL_PREFIX)
}

fn detail(text: &str) -> Span<'static> {
    Span::styled(
        format!("{DETAIL_SEPARATOR}{}", one_line(text)),
        theme::current().tool_dim,
    )
}

fn emitted_detail(emitted: usize) -> Option<Span<'static>> {
    (emitted > 0).then(|| detail(&format!("{emitted}{EMITTED_SUFFIX}")))
}

/// A string reads as itself, anything else as indented JSON.
fn result_rows(result: &Value, width: u16) -> Rows {
    let mut rows = Rows::default();
    match result {
        Value::Null => {}
        Value::String(text) => push_text(&mut rows, text, width),
        value => {
            if let Ok(pretty) = serde_json::to_string_pretty(value) {
                rows.push(json_body(&pretty, width).0);
            }
        }
    }
    rows
}

/// Executor reports a failure as a string, or as the `{code, message}` a
/// tool returned.
fn error_rows(error: &Value, width: u16) -> Rows {
    let mut rows = Rows::default();
    let field = |key| error.get(key).and_then(Value::as_str);
    let text = match (error, field(ERROR_CODE_KEY), field(ERROR_MESSAGE_KEY)) {
        (Value::String(text), _, _) => text.clone(),
        (_, Some(code), Some(message)) => format!("{code}{ERROR_CODE_SEPARATOR}{message}"),
        (_, None, Some(message)) => message.to_owned(),
        _ => return result_rows(error, width),
    };
    let style = theme::current().tool_error;
    rows.push(text.lines().map(|line| styled_line(line, style)).collect());
    rows
}

/// What a script emitted for the user leads the content. The envelope
/// Executor writes for the model follows it, when there is one.
fn push_emitted(rows: &mut Rows, output: &McpOutput, emitted: usize, envelope: bool, width: u16) {
    if emitted == 0 {
        return;
    }
    let content: Vec<&McpBlock> = output
        .blocks
        .iter()
        .filter(|block| !matches!(block, McpBlock::Structured { .. }))
        .collect();
    let items = content.len().saturating_sub(usize::from(envelope));
    rows.append_apart(blocks_rows(content.into_iter().take(items), width));
}

fn log_rows(logs: &[String], whole: bool) -> Rows {
    let mut rows = Rows::default();
    let lines: Vec<&str> = logs.iter().flat_map(|log| log.lines()).collect();
    if lines.is_empty() {
        return rows;
    }
    let style = theme::current().tool_dim;
    let shown = match whole {
        true => lines.len(),
        false => lines.len().min(MAX_LOG_LINES),
    };
    let mut drawn = vec![Line::from(Span::styled(LOGS_LABEL, style))];
    drawn.extend(
        lines[..shown]
            .iter()
            .map(|line| styled_line(&format!("{LOG_INDENT}{line}"), style)),
    );
    let hidden = lines.len() - shown;
    if hidden > 0 {
        drawn.push(styled_line(
            &format!("{LOG_INDENT}{MORE_LOGS_PREFIX}{hidden}{MORE_LOGS_SUFFIX}"),
            style,
        ));
    }
    rows.push(drawn);
    rows
}

/// The question, the call it holds up, and for a browser flow the address
/// the user is to visit.
fn waiting_lines(interaction: &Interaction) -> Vec<Line<'static>> {
    let theme = theme::current();
    let mut lines = vec![styled_line(&one_line(&interaction.message), theme.tool)];
    if !interaction.address.is_empty() {
        let mut call = vec![Span::styled(one_line(&interaction.address), theme.accent)];
        if let Some(summary) = args_summary(&interaction.args) {
            call.push(Span::styled(
                format!("{ARGS_OPEN}{summary}{ARGS_CLOSE}"),
                theme.tool_dim,
            ));
        }
        lines.push(Line::from(call));
    }
    if let (InteractionKind::Url, Some(url)) = (&interaction.kind, &interaction.url) {
        lines.push(field_line(
            URL_LABEL,
            Span::styled(one_line(url), theme.tool_dim),
        ));
    }
    lines
}

/// The arguments as compact JSON on one row, cut short when long.
fn args_summary(args: &Value) -> Option<String> {
    let empty = match args {
        Value::Null => true,
        Value::Object(fields) => fields.is_empty(),
        _ => false,
    };
    if empty {
        return None;
    }
    let mut summary = one_line(&serde_json::to_string(args).ok()?);
    if let Some((cut, _)) = summary.char_indices().nth(MAX_ARGS_SUMMARY_CHARS) {
        summary.truncate(cut);
        summary.push(ELLIPSIS);
    }
    Some(summary)
}

fn styled_line(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(escape_terminal_controls(text), style))
}

#[cfg(test)]
mod tests {
    use caudra_agent::McpServerKind;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const WIDTH: u16 = 80;
    const RESULT_TEXT: &str = "42 issues open";
    const TOOL_NAME: &str = "github.issues.list";
    const ERROR_TEXT: &str = "TypeError: tools.github is undefined";
    const MESSAGE: &str = "Allow creating an issue?";
    const ADDRESS: &str = "github.issues.create";
    const APPROVAL_URL: &str = "https://executor.sh/approve/abc";
    const EXPIRY: &str = "2026-10-13T12:00:00Z";
    const ENVELOPE: &str = "Execution envelope for the model";
    const EMITTED_TEXT: &str = "emitted for the user";

    fn output(structured: Value, blocks: Vec<McpBlock>) -> McpOutput {
        McpOutput {
            blocks,
            structured: Some(structured),
            kind: Some(McpServerKind::Executor),
            ..McpOutput::default()
        }
    }

    fn envelope() -> Vec<McpBlock> {
        vec![McpBlock::Text {
            text: ENVELOPE.into(),
        }]
    }

    fn rendered(tool: &str, output: &McpOutput, whole: bool) -> Option<Vec<String>> {
        render(tool, output, WIDTH, whole)
            .map(|rows| rows.lines.iter().map(ToString::to_string).collect())
    }

    fn logs(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("log {n}")).collect()
    }

    #[test]
    fn a_completed_run_names_its_tool_then_its_result() {
        let structured = json!({
            "status": "completed", "result": RESULT_TEXT, "toolName": TOOL_NAME, "logs": [],
        });

        let rows = rendered(EXECUTE_TOOL, &output(structured, envelope()), false).unwrap();

        assert_eq!(
            rows,
            [
                format!("{COMPLETED_LABEL}{DETAIL_SEPARATOR}{TOOL_NAME}"),
                RESULT_TEXT.to_owned(),
            ]
        );
    }

    #[test]
    fn a_json_result_is_indented() {
        let structured = json!({"status": "completed", "result": {"open": 42}, "logs": []});

        let rows = rendered(EXECUTE_TOOL, &output(structured, envelope()), false).unwrap();

        assert_eq!(rows, [COMPLETED_LABEL, "{", "  \"open\": 42", "}"]);
    }

    #[test_case(json!(ERROR_TEXT), ERROR_TEXT ; "string")]
    #[test_case(json!({"code": "denied", "message": "no"}), "denied: no" ; "code_and_message")]
    fn an_error_run_says_why_in_the_error_colour(error: Value, shown: &str) {
        let structured = json!({"status": "error", "error": error, "logs": []});

        let rows = render(EXECUTE_TOOL, &output(structured, envelope()), WIDTH, false).unwrap();

        let error_style = theme::current().tool_error;
        assert_eq!(rows.lines[0].spans[0].style, error_style);
        assert_eq!(rows.lines[1].to_string(), shown);
        assert_eq!(rows.lines[1].spans[0].style, error_style);
    }

    #[test_case(json!({"kind": "form", "message": MESSAGE, "address": ADDRESS, "args": {"title": "bug"}}), None ; "form")]
    #[test_case(json!({"kind": "url", "message": MESSAGE, "address": ADDRESS, "args": {}, "url": APPROVAL_URL}), Some(APPROVAL_URL) ; "url")]
    fn a_paused_run_says_what_it_waits_for(interaction: Value, url: Option<&str>) {
        let structured = json!({
            "status": "waiting_for_interaction", "executionId": "exec_1",
            "expiresAt": EXPIRY, "ttlMs": 60000, "interaction": interaction,
        });

        let card = render(RESUME_TOOL, &output(structured, envelope()), WIDTH, false).unwrap();
        let rows: Vec<String> = card.lines.iter().map(ToString::to_string).collect();

        assert_eq!(
            rows[0],
            format!("{WAITING_LABEL}{DETAIL_SEPARATOR}{EXPIRES_PREFIX}{EXPIRY}")
        );
        assert_eq!(rows[1], MESSAGE);
        assert!(rows[2].starts_with(ADDRESS), "{rows:?}");
        assert_eq!(
            rows.iter().any(|row| row.contains(APPROVAL_URL)),
            url.is_some(),
            "{rows:?}"
        );
        assert!(card.links.rows.iter().flatten().all(Option::is_none));
    }

    #[test_case(MAX_LOG_LINES, false, MAX_LOG_LINES ; "within_the_fold")]
    #[test_case(MAX_LOG_LINES + 3, false, MAX_LOG_LINES ; "folded")]
    #[test_case(MAX_LOG_LINES + 3, true, MAX_LOG_LINES + 3 ; "opened_whole")]
    fn logs_fold_past_the_limit(count: usize, whole: bool, shown: usize) {
        let structured = json!({"status": "completed", "result": null, "logs": logs(count)});

        let rows = rendered(EXECUTE_TOOL, &output(structured, envelope()), whole).unwrap();

        let drawn = rows
            .iter()
            .filter(|row| row.starts_with(&format!("{LOG_INDENT}log ")))
            .count();
        assert_eq!(drawn, shown, "{rows:?}");
        let fold = format!(
            "{LOG_INDENT}{MORE_LOGS_PREFIX}{}{MORE_LOGS_SUFFIX}",
            count - shown
        );
        assert_eq!(rows.last() == Some(&fold), count > shown, "{rows:?}");
    }

    #[test]
    fn emitted_items_are_drawn_without_the_envelope() {
        let structured = json!({"status": "completed", "result": 1, "emitted": 1, "logs": []});
        let blocks = vec![
            McpBlock::Text {
                text: EMITTED_TEXT.into(),
            },
            McpBlock::Text {
                text: ENVELOPE.into(),
            },
        ];

        let rows = rendered(EXECUTE_TOOL, &output(structured, blocks), false).unwrap();

        assert_eq!(
            rows[0],
            format!("{COMPLETED_LABEL}{DETAIL_SEPARATOR}1{EMITTED_SUFFIX}")
        );
        assert!(rows.iter().any(|row| row == EMITTED_TEXT), "{rows:?}");
        assert!(rows.iter().all(|row| row != ENVELOPE), "{rows:?}");
    }

    #[test_case("integrations", json!({"status": "completed", "logs": []}) ; "another_tool")]
    #[test_case(EXECUTE_TOOL, json!({"status": "user_approval_required"}) ; "unknown_status")]
    #[test_case(EXECUTE_TOOL, json!({"status": "completed", "logs": "not a list"}) ; "malformed")]
    fn anything_else_is_left_to_the_generic_card(tool: &str, structured: Value) {
        assert!(rendered(tool, &output(structured, envelope()), false).is_none());
    }
}
