use caudra_agent::{TaskCard, ToolOutput, format_settled_duration};
use caudra_providers::TaskEventOrigin;
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::time::Duration;

use crate::{
    components::{
        code_view::{RowTarget, WrappedRows, body_window, truncation_line},
        escape_terminal_controls, format_compact,
        tool_display::task_details,
    },
    theme,
};

const INDENT: &str = "  ";
const LABEL_WIDTH: usize = 8;
const SEPARATOR: &str = " · ";
const BACKGROUND_BADGE: &str = " [background]";
const OPEN_CHAT: &str = " · open chat";

pub(crate) fn delivery(origin: &TaskEventOrigin, text: &str, width: u16) -> Vec<Line<'static>> {
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
    spans.push(Span::styled(OPEN_CHAT, t.accent));
    let mut lines = vec![Line::from(spans)];
    if !body.trim().is_empty() {
        lines.push(Line::default());
        lines.extend(body.trim().lines().map(|line| {
            Line::from(vec![
                Span::raw(INDENT),
                Span::styled(escape_terminal_controls(line), t.tool),
            ])
        }));
    }
    WrappedRows::new(lines, 0, width).lines()
}

pub(crate) fn render(
    tasks: &[TaskCard],
    budget: usize,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>, bool) {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
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
        let mut task_line = fact("Task", &task.task_id, t.tool);
        task_line.spans.push(Span::styled(OPEN_CHAT, t.accent));
        identity.extend(WrappedRows::new(vec![task_line], 1, width).lines());
        identity.extend(WrappedRows::new(vec![fact("Mode", &task.mode, t.tool)], 1, width).lines());
        let mut body = Vec::new();
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
        let details = task_details(task);
        if !details.trim().is_empty() {
            let section = match task.state.as_str() {
                "blocked" => "Blocked",
                "failed" => "Error",
                _ if task.result.is_some() || task.result_preview.is_some() => "Result",
                _ => "Update",
            };
            body.push(Line::default());
            body.push(Line::styled(section, status_style(&task.state)));
            body.extend(details.lines().map(|line| {
                Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled(escape_terminal_controls(line), t.tool),
                ])
            }));
        }
        let body = WrappedRows::new(body, 0, width).lines();
        let room = budget.saturating_sub(lines.len() + identity.len());
        let (shown, hidden) = body_window(body.len(), room);
        lines.extend(identity);
        lines.extend(body.into_iter().take(shown));
        if hidden > 0 {
            lines.push(truncation_line(hidden));
            truncated = true;
        }
        rows.resize(lines.len(), Some(RowTarget::Item(index)));
    }
    (lines, rows, truncated)
}

fn fact(label: &str, value: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{INDENT}{label:<LABEL_WIDTH$}  "),
            theme::current().tool_dim,
        ),
        Span::styled(escape_terminal_controls(value), style),
    ])
}

fn status_style(state: &str) -> Style {
    let t = theme::current();
    match state {
        "completed" | "succeeded" => t.tool_success,
        "failed" | "interrupted" => t.tool_error,
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

pub(crate) fn has_active(output: &ToolOutput) -> bool {
    match output {
        ToolOutput::Tasks(tasks) => tasks.iter().any(TaskCard::active),
        ToolOutput::Batch { entries, .. } => entries
            .iter()
            .any(|entry| entry.output.as_ref().is_some_and(has_active)),
        _ => false,
    }
}
