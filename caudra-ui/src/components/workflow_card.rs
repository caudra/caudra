//! The transcript card of a workflow run: how a `ToolOutput::WorkflowRun`
//! draws, what its header says, and which tool status its run maps to. One
//! place, so a card a slash command opened and a card the `workflow` tool
//! returned cannot drift apart.

use std::path::PathBuf;

use caudra_agent::types::{PhaseMark, WorkflowRunCard};
use caudra_workflow::{RosterState, RunStatus};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::components::code_view::RowTarget;
use crate::components::{ToolStatus, escape_terminal_controls, format_compact, format_elapsed};
use crate::markdown::text_to_painted;
use crate::theme;

/// The tool id of a card a slash command opened, ahead of its run id. A
/// card the `workflow` tool drew keeps the tool call's id and is found by
/// the run its output names.
pub(crate) const CARD_ID_PREFIX: &str = "workflow:";
pub(crate) const CARD_SUMMARY_PREFIX: &str = "workflow ";
const SEPARATOR: &str = " \u{b7} ";
const PHASE_ARROW: &str = " \u{203a} ";
const RUNNING_MARK: &str = "\u{25cf} ";
const FAILED_MARK: &str = "\u{2717} ";
const LOG_PREFIX: &str = "+";
pub(crate) const AGENTS_SUFFIX: &str = " agents";
pub(crate) const TOKENS_SUFFIX: &str = " tokens";
const SCRATCH_LABEL: &str = "Scratch file: ";
/// The one row target a card carries: a run has at most one scratch file.
const SCRATCH_ROW: usize = 0;
const PAUSED_LABEL: &str = "Paused: ";
const ERROR_LABEL: &str = "Error: ";
const BUDGET_LIMITED: &str = "Budget limited: resume it with a higher agent budget";

/// What a click on a card names: the run itself, or the scratch file its
/// result line lists.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CardHit {
    Run(String),
    ScratchFile(PathBuf),
}

pub(crate) fn card_id(run_id: &str) -> String {
    format!("{CARD_ID_PREFIX}{run_id}")
}

pub(crate) fn summary(card: &WorkflowRunCard) -> String {
    format!(
        "{CARD_SUMMARY_PREFIX}{}",
        escape_terminal_controls(&card.display_name)
    )
}

/// `active · Research · 3/128 agents · 41k tokens`, and how long a settled
/// run took.
pub(crate) fn annotation(card: &WorkflowRunCard) -> String {
    let mut text = format!(
        "{}{SEPARATOR}{}/{}{AGENTS_SUFFIX}{SEPARATOR}{}{TOKENS_SUFFIX}",
        card.headline(),
        card.usage.agents_admitted,
        card.agent_budget,
        format_compact(card.usage.tokens_used)
    );
    if card.status.is_terminal() {
        text.push_str(SEPARATOR);
        text.push_str(&format_elapsed(
            card.updated_at.saturating_sub(card.created_at),
        ));
    }
    text
}

/// A run that can still move is in progress; one that ended well succeeded;
/// anything else ended short of what it set out to do.
pub(crate) fn status(status: RunStatus) -> ToolStatus {
    match status {
        RunStatus::Active | RunStatus::Paused | RunStatus::BudgetLimited => ToolStatus::InProgress,
        RunStatus::Completed => ToolStatus::Success,
        RunStatus::Interrupted | RunStatus::Cancelled | RunStatus::Failed => ToolStatus::Error,
    }
}

/// The phase strip, the agents still working or that failed, the last log
/// lines while the run is going, and what it produced once it is not. The
/// rows run parallel to the lines and mark the one that lists the scratch
/// file, so a click can name it after a reflow. `width` is the columns the
/// card draws in, which the preview wraps its markdown into and zero leaves
/// as source.
pub(crate) fn render(
    card: &WorkflowRunCard,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>) {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut scratch_line = None;
    let strip = card.phase_strip();
    if !strip.is_empty() {
        lines.push(phase_strip_line(&strip));
    }
    for agent in &card.roster {
        let (mark, style) = match agent.state {
            RosterState::Running => (RUNNING_MARK, t.accent),
            RosterState::Failed => (FAILED_MARK, t.tool_error),
            RosterState::Pending | RosterState::Completed | RosterState::Cancelled => continue,
        };
        let mut spans = vec![
            Span::styled(mark, style),
            Span::raw(escape_terminal_controls(&agent.label)),
        ];
        if let Some(phase) = &agent.phase {
            spans.push(Span::styled(
                format!("{SEPARATOR}{}", escape_terminal_controls(phase)),
                t.tool_dim,
            ));
        }
        spans.push(Span::styled(
            format!(
                "{SEPARATOR}{}{TOKENS_SUFFIX}{SEPARATOR}{}",
                format_compact(agent.tokens_used),
                format_elapsed(agent.duration_ms / 1_000)
            ),
            t.tool_dim,
        ));
        lines.push(Line::from(spans));
    }
    if card.status.is_terminal() {
        if let Some(preview) = &card.result_preview {
            lines.extend(preview_lines(preview, width));
        }
        if let Some(path) = &card.scratch_path {
            scratch_line = Some(lines.len());
            lines.push(labelled(SCRATCH_LABEL, path, t.tool_path));
        }
    } else {
        for log in &card.logs {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "{LOG_PREFIX}{} ",
                        format_elapsed(log.at.saturating_sub(card.created_at))
                    ),
                    t.tool_dim,
                ),
                Span::raw(escape_terminal_controls(&log.message)),
            ]));
        }
    }
    if card.status == RunStatus::BudgetLimited {
        lines.push(Line::styled(BUDGET_LIMITED, t.tool_warning));
    }
    if let Some(message) = &card.pause_message {
        lines.push(labelled(PAUSED_LABEL, message, t.tool_warning));
    }
    if let Some(error) = &card.error {
        lines.push(labelled(ERROR_LABEL, error, t.tool_error));
    }
    let rows = (0..lines.len())
        .map(|line| (Some(line) == scratch_line).then_some(RowTarget(SCRATCH_ROW)))
        .collect();
    (lines, rows)
}

/// A run's preview is the head of a report a model wrote, so it is painted as
/// the markdown it is. A caller with no width to wrap into gets the source.
fn preview_lines(preview: &str, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return preview
            .lines()
            .map(|line| Line::raw(escape_terminal_controls(line)))
            .collect();
    }
    let style = theme::current().assistant;
    let (painted, _) = text_to_painted(
        preview,
        "",
        style,
        style,
        width,
        Some(caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES),
        Vec::new(),
    );
    painted.lines
}

pub(crate) fn phase_strip_line(strip: &[(String, PhaseMark)]) -> Line<'static> {
    let t = theme::current();
    let mut spans = Vec::with_capacity(strip.len() * 2);
    for (index, (title, mark)) in strip.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(PHASE_ARROW, t.tool_dim));
        }
        let style = match mark {
            PhaseMark::Done => t.tool_success,
            PhaseMark::Current => t.accent,
            PhaseMark::Pending => t.tool_dim,
        };
        spans.push(Span::styled(
            format!("{} {}", escape_terminal_controls(title), mark.glyph()),
            style,
        ));
    }
    Line::from(spans)
}

pub(crate) fn status_span(status: RunStatus) -> Span<'static> {
    let t = theme::current();
    let style = match status {
        RunStatus::Active => t.accent,
        RunStatus::Paused | RunStatus::BudgetLimited => t.tool_warning,
        RunStatus::Completed => t.tool_success,
        RunStatus::Interrupted | RunStatus::Cancelled | RunStatus::Failed => t.tool_error,
    };
    Span::styled(status.to_string(), style)
}

fn labelled(label: &'static str, text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, theme::current().tool_dim),
        Span::styled(escape_terminal_controls(text), style),
    ])
}

#[cfg(test)]
mod tests {
    use caudra_workflow::{AgentRosterEntry, LogLine, RunSnapshot, RunUsage, SourceKind};
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const RUN_ID: &str = "run-1";
    const REPORT: &str = "Findings: 42.";
    const LOG: &str = "searching";
    const LIVE_LOGS: &str = "a live card shows its log tail, a settled one its report";
    const SCRATCH_PATH: &str = "/state/workflow_scratch/session/run-1/report.md";
    const NO_SCRATCH_ROW: &str = "a card without a scratch file carries no row target";
    const CARD_COLS: u16 = 60;
    const MARKDOWN_PREVIEW: &str = "## Findings\n\n- 42 of them\n";
    const PAINTED_HEADING: &str = "Findings";
    const PAINTED_BULLET: &str = "\u{2022} 42 of them";
    const PAINTS_MARKDOWN: &str = "a card paints its preview as the markdown it is";
    const ONE_SCRATCH_ROW: &str = "the scratch line is the card's only row target";

    fn run(status: RunStatus) -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_ID.into(),
            display_name: "deep-research".into(),
            workflow_name: "deep-research".into(),
            source_kind: SourceKind::Builtin,
            source_path: None,
            objective: None,
            status,
            pause_kind: None,
            pause_message: None,
            revision: 1,
            execution_epoch: 1,
            phase: Some("Research".into()),
            phases: vec!["Plan".into(), "Research".into(), "Report".into()],
            phase_history: Vec::new(),
            agent_budget: 8,
            usage: RunUsage {
                agents_admitted: 3,
                tokens_used: 41_250,
            },
            roster: vec![AgentRosterEntry {
                call_key: 1,
                label: "researcher".into(),
                phase: None,
                task_id: None,
                state: RosterState::Running,
                tokens_used: 0,
                duration_ms: 0,
            }],
            result: Some(json!({ "report": REPORT })),
            error: None,
            logs: vec![LogLine {
                at: 10,
                message: LOG.into(),
            }],
            outbox_pending: false,
            created_at: 0,
            updated_at: 134,
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test_case(RunStatus::Active => "active · Research · 3/8 agents · 41k tokens" ; "live")]
    #[test_case(RunStatus::Completed => "completed · Research · 3/8 agents · 41k tokens · 2m14s" ; "settled_adds_its_duration")]
    fn the_annotation_reads_the_headline_and_usage(status: RunStatus) -> String {
        annotation(&WorkflowRunCard::from(&run(status)))
    }

    #[test_case(RunStatus::Active => ToolStatus::InProgress ; "active")]
    #[test_case(RunStatus::Paused => ToolStatus::InProgress ; "paused")]
    #[test_case(RunStatus::Completed => ToolStatus::Success ; "completed")]
    #[test_case(RunStatus::Cancelled => ToolStatus::Error ; "cancelled")]
    fn run_status_maps_to_a_tool_status(run_status: RunStatus) -> ToolStatus {
        status(run_status)
    }

    #[test]
    fn a_live_card_shows_the_strip_the_roster_and_the_log_tail() {
        let (lines, rows) = render(&WorkflowRunCard::from(&run(RunStatus::Active)), CARD_COLS);
        let body = text(&lines);

        assert!(rows.iter().all(Option::is_none), "{NO_SCRATCH_ROW}");
        assert!(body.starts_with("Plan ✓ › Research ● › Report ○"), "{body}");
        assert!(body.contains("● researcher"), "{body}");
        assert!(body.contains(&format!("+10s {LOG}")), "{LIVE_LOGS}: {body}");
        assert!(!body.contains(REPORT), "{LIVE_LOGS}: {body}");
    }

    #[test]
    fn a_settled_card_shows_the_report_instead_of_the_logs() {
        let (lines, rows) = render(
            &WorkflowRunCard::from(&run(RunStatus::Completed)),
            CARD_COLS,
        );
        let body = text(&lines);

        assert!(body.contains(REPORT), "{LIVE_LOGS}: {body}");
        assert!(!body.contains(LOG), "{LIVE_LOGS}: {body}");
        assert!(rows.iter().all(Option::is_none), "{NO_SCRATCH_ROW}");
    }

    /// A report is markdown a model wrote, and a card that shows its head
    /// shows it the way the transcript shows every other model answer.
    #[test]
    fn a_settled_card_paints_its_preview_as_markdown() {
        let mut settled = run(RunStatus::Completed);
        settled.result = Some(json!({ "report": MARKDOWN_PREVIEW }));

        let (lines, _) = render(&WorkflowRunCard::from(&settled), CARD_COLS);

        let body = text(&lines);
        assert!(body.contains(PAINTED_HEADING), "{PAINTS_MARKDOWN}: {body}");
        assert!(body.contains(PAINTED_BULLET), "{PAINTS_MARKDOWN}: {body}");
    }

    #[test]
    fn the_scratch_line_of_a_settled_card_is_its_one_row_target() {
        let mut settled = run(RunStatus::Completed);
        settled.result = Some(json!({ "report": REPORT, "path": SCRATCH_PATH }));

        let (lines, rows) = render(&WorkflowRunCard::from(&settled), CARD_COLS);

        let targets: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(line, row)| row.map(|_| line))
            .collect();
        assert_eq!(targets.len(), 1, "{ONE_SCRATCH_ROW}");
        assert!(
            text(&lines[targets[0]..=targets[0]]).ends_with(SCRATCH_PATH),
            "{ONE_SCRATCH_ROW}"
        );
    }
}
