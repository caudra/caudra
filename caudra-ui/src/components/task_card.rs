use caudra_agent::{TaskCard, ToolOutput, format_settled_duration};
use caudra_providers::TaskEventOrigin;
use caudra_storage::background::{JobKind, JobOwner};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::time::Duration;

use crate::{
    components::{
        code_view::{RowTarget, WrappedRows, body_window, plain_body, truncation_line},
        escape_terminal_controls, format_compact,
        tool_display::task_details,
    },
    markdown::{LinkMap, text_to_wrapped},
    theme,
};

const INDENT: &str = "  ";
const LABEL_WIDTH: usize = 8;
const SEPARATOR: &str = " · ";
const BACKGROUND_BADGE: &str = " [background]";
const OPEN_CHAT: &str = " · open chat";

pub(crate) fn is_shell_delivery(origin: &TaskEventOrigin, text: &str) -> bool {
    text.starts_with(&format!("Shell {}:", origin.task_id))
}

pub(crate) fn literal_body(text: &str, width: u16) -> (Vec<Line<'static>>, LinkMap) {
    let text = text
        .lines()
        .map(escape_terminal_controls)
        .collect::<Vec<_>>()
        .join("\n");
    let (mut lines, _) = plain_body(&text, width.saturating_sub(INDENT.len() as u16).max(1));
    for line in &mut lines {
        line.style = theme::current().tool;
        line.spans.insert(0, Span::raw(INDENT));
    }
    let links = LinkMap::none_for(&lines);
    (lines, links)
}

pub(crate) fn details(task: &TaskCard, width: u16) -> (Vec<Line<'static>>, LinkMap) {
    match task.kind {
        JobKind::Agent => markdown_body(&task_details(task), width),
        JobKind::Shell => literal_body(&task_details(task), width),
    }
}

fn shell_facts(task: &TaskCard, width: u16) -> Vec<Line<'static>> {
    let Some(shell) = &task.shell else {
        return Vec::new();
    };
    let t = theme::current();
    let owner = match task.owner {
        JobOwner::Main => "main chat",
        JobOwner::Child { .. } => "agent task",
    };
    WrappedRows::new(
        vec![
            fact("Command", &shell.command, t.tool),
            fact("Owner", owner, t.tool),
            fact("Workdir", &shell.workdir, t.tool),
            fact(
                "Timeout",
                &format_settled_duration(Duration::from_millis(shell.timeout_ms)),
                t.tool,
            ),
        ],
        0,
        width,
    )
    .lines()
}

pub(crate) fn markdown_body(text: &str, width: u16) -> (Vec<Line<'static>>, LinkMap) {
    let escaped = text
        .lines()
        .map(escape_terminal_controls)
        .collect::<Vec<_>>()
        .join("\n");
    let (mut painted, _) = text_to_wrapped(
        &escaped,
        theme::current().assistant,
        width.saturating_sub(INDENT.len() as u16).max(1),
        caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES,
    );
    for (line, links) in painted.lines.iter_mut().zip(&mut painted.links.rows) {
        line.spans.insert(0, Span::raw(INDENT));
        links.insert(0, None);
    }
    (painted.lines, painted.links)
}

pub(crate) fn delivery(
    origin: &TaskEventOrigin,
    text: &str,
    width: u16,
) -> (Vec<Line<'static>>, LinkMap) {
    let t = theme::current();
    let (heading, body) = text.split_once('\n').unwrap_or((text, ""));
    let heading = if heading.trim().is_empty() {
        format!("Task {}", origin.task_id)
    } else {
        escape_terminal_controls(heading)
    };
    let mut spans = match heading.rsplit_once(": ") {
        Some((label, state)) => vec![
            Span::styled(format!("{label}: "), t.tool_prefix),
            Span::styled(
                state.to_owned(),
                match state.trim_end_matches('.') {
                    "success" => t.tool_success,
                    "failure" => t.tool_error,
                    "blocked" => t.tool_warning,
                    _ => t.tool_dim,
                },
            ),
        ],
        None => vec![Span::styled(heading, t.tool_prefix)],
    };
    let shell = is_shell_delivery(origin, text);
    if !shell {
        spans.push(Span::styled(OPEN_CHAT, t.accent));
    }
    let mut lines = WrappedRows::new(vec![Line::from(spans)], 0, width).lines();
    let mut links = LinkMap::none_for(&lines);
    if !body.trim().is_empty() {
        lines.push(Line::default());
        links.rows.push(Vec::new());
        let (body, body_links) = if shell {
            literal_body(body.trim(), width)
        } else {
            markdown_body(body.trim(), width)
        };
        lines.extend(body);
        links.rows.extend(body_links.rows);
    }
    (lines, links)
}

pub(crate) fn render(
    tasks: &[TaskCard],
    budget: usize,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>, bool, LinkMap) {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut links = LinkMap::default();
    for (index, task) in tasks.iter().enumerate() {
        if index > 0 {
            if lines.len() >= budget {
                lines.push(truncation_line(tasks.len() - index));
                rows.push(None);
                truncated = true;
                break;
            }
            lines.push(Line::default());
            rows.push(None);
        }
        let mut heading = vec![
            Span::styled(escape_terminal_controls(&task.label), t.tool_prefix),
            Span::styled(SEPARATOR, t.tool_dim),
            Span::styled(
                escape_terminal_controls(&task.state),
                status_style(&task.state),
            ),
        ];
        if task.background {
            heading.push(Span::styled(BACKGROUND_BADGE, t.accent));
        }
        let mut identity = WrappedRows::new(vec![Line::from(heading)], 0, width).lines();
        let mut task_line = fact(
            if task.kind == JobKind::Shell {
                "Shell"
            } else {
                "Task"
            },
            &task.task_id,
            t.tool,
        );
        if task.kind == JobKind::Agent {
            task_line.spans.push(Span::styled(OPEN_CHAT, t.accent));
        }
        identity.extend(WrappedRows::new(vec![task_line], 1, width).lines());
        identity.extend(WrappedRows::new(vec![fact("Mode", &task.mode, t.tool)], 1, width).lines());
        let mut body = shell_facts(task, width);
        if let Some(result) = &task.result {
            if let Some(duration) = result.get("duration_ms").and_then(|value| value.as_u64()) {
                body.push(fact(
                    "Duration",
                    &format_settled_duration(Duration::from_millis(duration)),
                    t.tool,
                ));
            }
            if let Some(tokens) = result.get("tokens_used").and_then(|value| value.as_u64()) {
                body.push(fact(
                    "Usage",
                    &format!("{} tokens", format_compact(tokens)),
                    t.tool,
                ));
            }
        }
        let mut body = WrappedRows::new(body, 0, width).lines();
        let mut body_links = LinkMap::none_for(&body);
        let details = task_details(task);
        if !details.trim().is_empty() {
            let section = match task.state.as_str() {
                "blocked" => "Blocked",
                "failed" => "Error",
                _ if task.result.is_some() || task.result_preview.is_some() => "Result",
                _ => "Update",
            };
            body.push(Line::default());
            body_links.rows.push(Vec::new());
            let heading = WrappedRows::new(
                vec![Line::styled(section, status_style(&task.state))],
                0,
                width,
            )
            .lines();
            body_links.rows.extend(LinkMap::none_for(&heading).rows);
            body.extend(heading);
            let (details, detail_links) = self::details(task, width);
            body.extend(details);
            body_links.rows.extend(detail_links.rows);
        }
        let room = budget.saturating_sub(lines.len() + identity.len());
        let (shown, hidden) = body_window(body.len(), room);
        lines.extend(identity);
        links
            .rows
            .extend(LinkMap::none_for(&lines[links.rows.len()..]).rows);
        lines.extend(body.into_iter().take(shown));
        links.rows.extend(body_links.rows.into_iter().take(shown));
        if hidden > 0 {
            lines.push(truncation_line(hidden));
            truncated = true;
        }
        rows.resize(
            lines.len(),
            (task.kind == JobKind::Agent).then_some(RowTarget::Item(index)),
        );
    }
    links
        .rows
        .extend(LinkMap::none_for(&lines[links.rows.len()..]).rows);
    (lines, rows, truncated, links)
}

pub(crate) fn fact(label: &str, value: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{INDENT}{label:<LABEL_WIDTH$}  "),
            theme::current().tool_dim,
        ),
        Span::styled(escape_terminal_controls(value), style),
    ])
}

pub(crate) fn status_style(state: &str) -> Style {
    let t = theme::current();
    match state {
        "completed" | "succeeded" => t.tool_success,
        "failed" | "interrupted" | "timed_out" | "indeterminate" => t.tool_error,
        "blocked" | "cancelled" | "cancelling" => t.tool_warning,
        "running" => t.accent,
        _ => t.tool_dim,
    }
}

fn task_count(output: &ToolOutput) -> usize {
    match output {
        ToolOutput::Tasks(tasks) => tasks.len(),
        ToolOutput::Batch { entries, .. } => entries
            .iter()
            .filter_map(|entry| entry.output.as_ref())
            .map(task_count)
            .sum(),
        _ => 0,
    }
}

pub(crate) fn target_index(output: &ToolOutput, target: RowTarget) -> Option<usize> {
    match output {
        ToolOutput::Tasks(tasks) => (target.index() < tasks.len()).then_some(target.index()),
        ToolOutput::Batch { entries, .. } => {
            let child = target.index();
            let output = entries.get(child)?.output.as_ref()?;
            let task = match target {
                RowTarget::Task { task, .. } => task,
                RowTarget::Item(_) if task_count(output) == 1 => 0,
                _ => return None,
            };
            Some(
                entries[..child]
                    .iter()
                    .filter_map(|entry| entry.output.as_ref())
                    .map(task_count)
                    .sum::<usize>()
                    + task,
            )
        }
        _ => None,
    }
}

pub(crate) fn task_at(output: &ToolOutput, mut index: usize) -> Option<&TaskCard> {
    match output {
        ToolOutput::Tasks(tasks) => tasks.get(index),
        ToolOutput::Batch { entries, .. } => {
            for output in entries.iter().filter_map(|entry| entry.output.as_ref()) {
                let count = task_count(output);
                if index < count {
                    return task_at(output, index);
                }
                index -= count;
            }
            None
        }
        _ => None,
    }
}

pub(crate) fn contains_invocation(output: &ToolOutput, invocation: &str) -> bool {
    match output {
        ToolOutput::Tasks(tasks) => tasks.iter().any(|task| task.invocation_id == invocation),
        ToolOutput::Batch { entries, .. } => entries.iter().any(|entry| {
            entry
                .output
                .as_ref()
                .is_some_and(|output| contains_invocation(output, invocation))
        }),
        _ => false,
    }
}

pub(crate) fn find_call<'a>(output: &'a ToolOutput, call_id: &str) -> Option<&'a TaskCard> {
    match output {
        ToolOutput::Tasks(tasks) => tasks.iter().find(|task| task.call_id == call_id),
        ToolOutput::Batch { entries, .. } => entries
            .iter()
            .filter_map(|entry| entry.output.as_ref())
            .find_map(|output| find_call(output, call_id)),
        _ => None,
    }
}

pub(crate) fn has_active(output: &ToolOutput) -> bool {
    match output {
        ToolOutput::Tasks(tasks) => tasks.iter().any(TaskCard::active),
        ToolOutput::Batch { entries, .. } => entries
            .iter()
            .any(|entry| entry.output.as_ref().is_some_and(has_active)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{delivery, markdown_body, render};
    use crate::chat::history_to_display;
    use crate::components::{DisplayRole, code_view::RowTarget};
    use caudra_agent::{History, TaskCard};
    use caudra_providers::{Message, TaskEventOrigin};
    use caudra_storage::background::{JobKind, ShellJobMetadata};
    use ratatui::{style::Modifier, text::Line};
    use serde_json::{Value, json};
    use std::slice;
    use test_case::test_case;

    const LINK: &str = "https://example.com/task";
    const MARKDOWN: &str = "# Findings\n\n**Strong** and `code` with [docs](https://example.com/task).\n\n- first\n- second\n\n```rust\nlet value = 1;\n```\n\n| Name | Value |\n| --- | --- |\n| a | b |";
    const RUST_SIGNATURE: &str = "TransferSession::preview(&self, path: &WorkspacePath, cancel: &CancelToken) -> Result<TransferPreview, TransferError>";
    const INLINE_TYPE: &str = "Option<FilePreview>";
    const CODE: &str = "fn preview<T>() -> Option<T> { None }";
    const LITERAL_ENTITIES: &str = "&lt;literal&gt;";

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn task(output: Value) -> TaskCard {
        serde_json::from_value(json!({
            "task_id": "readable-task", "invocation_id": "invocation", "call_id": "call",
            "root_call_id": "call", "label": "Inspect", "state": "succeeded", "mode": "build",
            "background": false, "generation": 1, "created_at": 1, "updated_at": 2,
            "result": {"output": output, "duration_ms": 2000, "tokens_used": 1000}
        }))
        .unwrap()
    }

    #[test_case(32; "narrow")]
    #[test_case(80; "wide")]
    fn markdown_layout_keeps_styles_links_and_width(width: u16) {
        let (lines, links) = markdown_body(MARKDOWN, width);
        let shown = text(&lines);
        assert!(!shown.contains("**Strong**"));
        assert!(!shown.contains("```rust"));
        assert!(shown.contains("let value = 1;"));
        assert!(lines.iter().all(|line| line.width() <= width as usize));
        assert!(lines.iter().flat_map(|line| &line.spans).any(
            |span| span.content == "Strong" && span.style.add_modifier.contains(Modifier::BOLD)
        ));
        assert!(links.is_aligned(&lines));
        assert!(
            links
                .rows
                .iter()
                .flatten()
                .any(|link| link.as_deref() == Some(LINK))
        );
    }

    #[test_case(32; "narrow")]
    #[test_case(80; "wide")]
    fn task_budget_counts_markdown_rows_and_preserves_targets(width: u16) {
        let task = task(json!(MARKDOWN));
        let (all, _, _, _) = render(slice::from_ref(&task), usize::MAX, width);
        let budget = all.len() - 2;
        let (lines, targets, truncated, links) = render(&[task], budget, width);
        assert!(truncated);
        assert_eq!(lines.len(), budget);
        assert!(links.is_aligned(&lines));
        assert_eq!(targets, vec![Some(RowTarget::Item(0)); lines.len()]);
        assert!(text(&lines).contains("Duration"));
        assert!(text(&lines).contains("Usage"));
    }

    #[test_case(true; "structured_result")]
    #[test_case(false; "report")]
    fn results_and_reports_keep_markdown_distinct_from_json(structured: bool) {
        const LITERAL: &str = "**literal**";
        let mut task = task(json!({"note": LITERAL}));
        if !structured {
            task.result = None;
            task.reports = vec!["**report**".into()];
        }
        let (lines, _, _, _) = render(slice::from_ref(&task), usize::MAX, 80);
        let shown = text(&lines);
        if structured {
            assert!(shown.contains(LITERAL));
            assert!(!shown.contains("```"));
        } else {
            assert!(shown.contains("report"));
            assert!(!shown.contains("**report**"));
        }
    }

    #[test_case(32; "wrapped_heading")]
    #[test_case(80; "unwrapped_heading")]
    fn delivery_keeps_attribution_and_markdown_links(width: u16) {
        let origin = TaskEventOrigin {
            task_id: "readable-task".into(),
            invocation_id: "private-invocation".into(),
            event_id: "private-event".into(),
        };
        let (lines, links) = delivery(
            &origin,
            &format!("Task readable-task: success.\n\n{MARKDOWN}"),
            width,
        );
        let shown = text(&lines);
        assert!(shown.contains("open chat"));
        assert!(!shown.contains("private-"));
        assert!(!shown.contains("**Strong**"));
        assert!(links.is_aligned(&lines));
        assert!(
            links
                .rows
                .iter()
                .flatten()
                .any(|link| link.as_deref() == Some(LINK))
        );
    }

    #[test_case("\u{1b}[31m**red**\u{7}"; "terminal_controls")]
    #[test_case("# café 界\n\n**résumé**"; "unicode")]
    fn markdown_is_terminal_safe(input: &str) {
        let (lines, links) = markdown_body(input, 24);
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .all(|span| !span.content.chars().any(char::is_control))
        }));
        assert!(links.is_aligned(&lines));
    }

    #[test_case(false; "live")]
    #[test_case(true; "restored")]
    fn task_delivery_preserves_rust_syntax_and_literal_entities(restored: bool) {
        let origin = TaskEventOrigin {
            task_id: "readable-task".into(),
            invocation_id: "private-invocation".into(),
            event_id: "private-event".into(),
        };
        let source = format!(
            "Task readable-task: report.\n\n**API**: {RUST_SIGNATURE}\n\n`{INLINE_TYPE}` & `{LITERAL_ENTITIES}`\n\n```rust\n{CODE}\n```\n\n[docs]({LINK})"
        );
        let items = History::new(vec![Message::task_observation(
            source.clone(),
            origin.clone(),
        )])
        .into_items();
        let items = if restored {
            serde_json::from_slice(&serde_json::to_vec(&items).unwrap()).unwrap()
        } else {
            items
        };
        let (messages, pending) =
            history_to_display(&items, &Default::default(), &Default::default(), false);
        assert!(pending.is_empty());
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.text, source);
        let DisplayRole::TaskDelivery(actual_origin) = &message.role else {
            panic!("expected attributed task delivery");
        };
        assert_eq!(actual_origin.as_ref(), &origin);
        let (lines, links) = delivery(actual_origin, &message.text, 200);
        let shown = text(&lines);
        for expected in [
            RUST_SIGNATURE,
            INLINE_TYPE,
            CODE,
            LITERAL_ENTITIES,
            "open chat",
        ] {
            assert!(shown.contains(expected), "missing {expected}: {shown}");
        }
        assert!(!shown.contains("**API**"));
        assert!(!shown.contains("private-"));
        assert!(links.is_aligned(&lines));
        assert!(
            links
                .rows
                .iter()
                .flatten()
                .any(|link| link.as_deref() == Some(LINK))
        );
    }

    #[test_case(false; "card")]
    #[test_case(true; "delivery")]
    fn shell_output_is_literal_without_chat_or_link_targets(delivered: bool) {
        const OUTPUT: &str = "**literal** `code` [link](https://example.com/task)";
        let mut card = task(json!(OUTPUT));
        card.kind = JobKind::Shell;
        card.shell = Some(Box::new(ShellJobMetadata {
            call_id: card.call_id.clone(),
            root_call_id: card.root_call_id.clone(),
            command: "printf output".into(),
            workdir: ".".into(),
            timeout_ms: 120_000,
            mode: "build".into(),
        }));
        let (lines, links) = if delivered {
            let origin = TaskEventOrigin {
                task_id: card.task_id.clone(),
                invocation_id: card.invocation_id.clone(),
                event_id: "event".into(),
            };
            delivery(
                &origin,
                &format!("Shell {}: success.\n\n{OUTPUT}", card.task_id),
                100,
            )
        } else {
            let (lines, targets, _, links) = render(&[card], usize::MAX, 100);
            assert!(targets.iter().all(Option::is_none));
            assert!(text(&lines).contains("Workdir"));
            assert!(text(&lines).contains("Timeout"));
            (lines, links)
        };
        assert!(text(&lines).contains(OUTPUT));
        assert!(!text(&lines).contains("open chat"));
        assert!(links.is_aligned(&lines));
        assert!(links.rows.iter().flatten().all(Option::is_none));
    }
}
