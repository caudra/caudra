//! The transcript card of an `execution_environment` result: the host written
//! for a reader, rather than the descriptor it was collected as.
//!
//! Two lines carry the answer — what the host is, and what a command runs in —
//! so they are pinned and everything else is what a budget takes away.

use caudra_agent::{
    ENVIRONMENT_COMMANDS_LABEL, ENVIRONMENT_MISSING_LABEL, ENVIRONMENT_NO_VERSION,
    EnvironmentCommand, EnvironmentFact,
};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::code_view::{truncation_line, within};
use crate::components::escape_terminal_controls;
use crate::theme;

const INDENT: &str = "  ";
/// Columns between a label and its value, and between two command cells.
const GAP: usize = 2;
const LIST_SEPARATOR: &str = ", ";
/// The headline and the summary, which a collapsed card shows before anything
/// it was asked to leave out.
const PINNED_LINES: usize = 2;

/// The card's lines and whether anything was left out of them.
pub(crate) fn render(
    headline: &str,
    summary: &str,
    facts: &[EnvironmentFact],
    commands: &[EnvironmentCommand],
    budget: usize,
    width: u16,
) -> (Vec<Line<'static>>, bool) {
    let theme = theme::current();
    let present: Vec<&EnvironmentCommand> = commands.iter().filter(|c| c.available).collect();
    let missing: Vec<&str> = commands
        .iter()
        .filter(|command| !command.available)
        .map(|command| command.id.as_str())
        .collect();

    let label_width = facts
        .iter()
        .map(|fact| fact.label.width())
        .chain((!present.is_empty()).then(|| ENVIRONMENT_COMMANDS_LABEL.width()))
        .chain((!missing.is_empty()).then(|| ENVIRONMENT_MISSING_LABEL.width()))
        .max()
        .unwrap_or_default();
    let value_width = (width as usize).saturating_sub(INDENT.width() + label_width + GAP);

    let mut body: Vec<Line<'static>> = Vec::new();
    for fact in facts {
        body.extend(labelled(
            &fact.label,
            label_width,
            vec![vec![Span::styled(
                escape_terminal_controls(&fact.value),
                theme.tool,
            )]],
        ));
    }
    if !present.is_empty() {
        body.extend(labelled(
            ENVIRONMENT_COMMANDS_LABEL,
            label_width,
            command_grid(&present, value_width),
        ));
    }
    if !missing.is_empty() {
        let rows = wrap_list(&missing, value_width)
            .into_iter()
            .map(|row| vec![Span::styled(row, theme.tool_dim)])
            .collect();
        body.extend(labelled(ENVIRONMENT_MISSING_LABEL, label_width, rows));
    }

    let (shown, hidden) = body_window(body.len(), budget.saturating_sub(PINNED_LINES));
    let mut lines = vec![
        Line::from(Span::styled(escape_terminal_controls(headline), theme.tool)),
        Line::from(Span::styled(
            escape_terminal_controls(summary),
            theme.tool_dim,
        )),
    ];
    lines.extend(body.into_iter().take(shown));
    if hidden > 0 {
        lines.push(truncation_line(hidden));
    }
    (lines, hidden > 0)
}

/// How much of the body fits, given that the notice about the rest costs a row
/// of its own.
fn body_window(total: usize, room: usize) -> (usize, usize) {
    let (shown, hidden) = within(total, room);
    if hidden == 0 {
        return (shown, hidden);
    }
    within(total, room.saturating_sub(1))
}

/// The label leads its first row; the rows under it line up beneath the value.
fn labelled(label: &str, label_width: usize, rows: Vec<Vec<Span<'static>>>) -> Vec<Line<'static>> {
    let theme = theme::current();
    let blank = " ".repeat(INDENT.width() + label_width + GAP);
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = vec![if index == 0 {
                Span::styled(
                    format!("{INDENT}{label:<label_width$}{}", " ".repeat(GAP)),
                    theme.tool_dim,
                )
            } else {
                Span::raw(blank.clone())
            }];
            spans.extend(row);
            Line::from(spans)
        })
        .collect()
}

/// Commands in even columns, id and version styled apart so a version reads as
/// a measurement rather than as part of the name. A width of zero means the
/// caller does not want the rows wrapped, so they all sit on one.
fn command_grid(commands: &[&EnvironmentCommand], width: usize) -> Vec<Vec<Span<'static>>> {
    let theme = theme::current();
    let cells: Vec<(String, String)> = commands
        .iter()
        .map(|command| {
            (
                escape_terminal_controls(&command.id),
                command.version.as_deref().map_or_else(
                    || ENVIRONMENT_NO_VERSION.to_owned(),
                    escape_terminal_controls,
                ),
            )
        })
        .collect();
    let cell_width = cells
        .iter()
        .map(|(id, version)| id.width() + 1 + version.width())
        .max()
        .unwrap_or_default()
        + GAP;
    let columns = if width == 0 {
        cells.len().max(1)
    } else {
        (width / cell_width).max(1)
    };

    cells
        .chunks(columns)
        .map(|chunk| {
            let mut spans = Vec::with_capacity(chunk.len() * 3);
            for (index, (id, version)) in chunk.iter().enumerate() {
                spans.push(Span::styled(id.clone(), theme.tool));
                spans.push(Span::styled(format!(" {version}"), theme.tool_dim));
                if index + 1 < chunk.len() {
                    let used = id.width() + 1 + version.width();
                    spans.push(Span::raw(" ".repeat(cell_width.saturating_sub(used))));
                }
            }
            spans
        })
        .collect()
}

/// A comma-separated list broken at the card's width. Zero means one row.
fn wrap_list(items: &[&str], width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for item in items {
        let escaped = escape_terminal_controls(item);
        match rows.last_mut() {
            Some(row)
                if width == 0
                    || row.width() + LIST_SEPARATOR.width() + escaped.width() <= width =>
            {
                row.push_str(LIST_SEPARATOR);
                row.push_str(&escaped);
            }
            _ => rows.push(escaped),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const HEADLINE: &str = "ubuntu 24.04 · linux/x86_64";
    const SUMMARY: &str = "bash · container sandbox · not root";
    const WIDTH: u16 = 80;

    fn command(id: &str, available: bool, version: Option<&str>) -> EnvironmentCommand {
        EnvironmentCommand {
            id: id.to_owned(),
            available,
            version: version.map(str::to_owned),
        }
    }

    fn fixture() -> (Vec<EnvironmentFact>, Vec<EnvironmentCommand>) {
        (
            vec![
                EnvironmentFact {
                    label: "runtime".into(),
                    value: "workcell-mcp 0.1.0".into(),
                },
                EnvironmentFact {
                    label: "packages".into(),
                    value: "apt 2.8.3".into(),
                },
            ],
            vec![
                command("bash", true, Some("5.2.21")),
                command("kubectl", true, None),
                command("zsh", false, None),
            ],
        )
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn a_collapsed_card_keeps_the_two_lines_that_answer_the_question() {
        let (facts, commands) = fixture();
        let (lines, truncated) = render(HEADLINE, SUMMARY, &facts, &commands, 3, WIDTH);

        assert!(truncated);
        assert_eq!(lines.len(), 3);
        assert_eq!(text(&lines[0]), HEADLINE);
        assert_eq!(text(&lines[1]), SUMMARY);
        assert!(text(&lines[2]).contains('4'), "{}", text(&lines[2]));
    }

    #[test]
    fn an_expanded_card_draws_every_fact_and_no_notice() {
        let (facts, commands) = fixture();
        let (lines, truncated) = render(HEADLINE, SUMMARY, &facts, &commands, usize::MAX, WIDTH);

        assert!(!truncated);
        let body: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(body.len(), 6);
        assert!(body[2].trim_start().starts_with("runtime"));
        assert!(body[2].ends_with("workcell-mcp 0.1.0"));
        assert!(body[3].ends_with("apt 2.8.3"));
    }

    #[test]
    fn an_installed_command_without_a_version_is_not_reported_missing() {
        let (facts, commands) = fixture();
        let (lines, _) = render(HEADLINE, SUMMARY, &facts, &commands, usize::MAX, WIDTH);
        let body: Vec<String> = lines.iter().map(text).collect();

        let commands_row = body
            .iter()
            .find(|row| row.contains(ENVIRONMENT_COMMANDS_LABEL))
            .expect("a commands row");
        assert!(commands_row.contains(&format!("kubectl {ENVIRONMENT_NO_VERSION}")));
        let missing_row = body
            .iter()
            .find(|row| row.contains(ENVIRONMENT_MISSING_LABEL))
            .expect("a missing row");
        assert!(missing_row.contains("zsh"));
        assert!(!missing_row.contains("kubectl"));
    }

    #[test_case(80, 1 ; "one_row_when_the_cells_fit")]
    #[test_case(30, 3 ; "one_row_per_cell_when_they_do_not")]
    fn a_grid_wraps_to_the_card_width(width: u16, rows: usize) {
        let commands = [
            command("python3", true, Some("3.13.15")),
            command("node", true, Some("24.19.0")),
            command("docker", true, Some("29.7.2")),
        ];
        let present: Vec<&EnvironmentCommand> = commands.iter().collect();
        let value_width = (width as usize).saturating_sub(INDENT.width() + GAP);

        assert_eq!(command_grid(&present, value_width).len(), rows);
    }
}
