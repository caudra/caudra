//! The `code` tool's card under its snippet: a row for each server call the snippet made, which
//! opens to that call's own MCP card, then what the snippet printed, the value it ended on, and
//! why it stopped when it did not finish.

use caudra_agent::code::output::{CodeCall, CodeCallStatus, CodeOutcome, CodeOutput};
use caudra_agent::tools::ToolEffect;
use caudra_agent::{
    BatchToolEntry, BatchToolStatus, MCP_SUMMARY_PREFIX, McpBlock, McpHelper, McpOutput,
    ToolOutput, pretty_json,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::code_view::{BodySource, body_window, json_body, plain_body, truncation_line};
use super::mcp_card::decisions;
use crate::theme;

const PRINTED_LABEL: &str = "Printed";
const RESULT_LABEL: &str = "Result";
const DID_NOT_RUN: &str = "Did not run: ";
const STOPPED: &str = "Stopped: ";
const CANCELLED: &str = "Cancelled";
const KIND_SEPARATOR: &str = ": ";
const REPAIRED: &str = "Repaired before running: ";
const REPAIR_SEPARATOR: &str = ", ";

/// A run of the card's lines, and the text a selection over them copies. `None` is chrome.
pub(crate) type Block = (Vec<Line<'static>>, Option<BodySource>);

/// Each call as a batch child, so its row reads, folds and opens as a direct MCP call's card
/// would.
pub(crate) fn entries(output: &CodeOutput) -> Vec<BatchToolEntry> {
    output.calls.iter().map(entry).collect()
}

fn entry(call: &CodeCall) -> BatchToolEntry {
    let tool = call.qualified_name();
    let status = match call.status {
        CodeCallStatus::Running => BatchToolStatus::Running,
        CodeCallStatus::Success => BatchToolStatus::Success,
        CodeCallStatus::Error | CodeCallStatus::Refused => BatchToolStatus::Error,
    };
    let output = shown_output(call);
    let summary = match call.helper {
        Some(helper) => helper_summary(call, helper),
        None => format!("{MCP_SUMMARY_PREFIX}{tool}"),
    };
    BatchToolEntry {
        summary,
        tool,
        effect: ToolEffect::Unknown,
        status,
        input: None,
        raw_input: Some(call.input.clone()),
        annotation: output.as_ref().and_then(ToolOutput::annotation),
        output,
        model_suffix: None,
        refused: call.status == CodeCallStatus::Refused,
    }
}

/// What a call's row opens to: the server's answer when it gave one the call stands on, else why
/// the call failed, drawn as an MCP card whenever the server asked the user something on the way.
fn shown_output(call: &CodeCall) -> Option<ToolOutput> {
    let interactions = call.interactions.clone();
    if let Some(output) = &call.output
        && (call.error.is_none() || output.is_error)
    {
        return Some(ToolOutput::Mcp(McpOutput {
            interactions,
            helper: call.helper,
            ..output.clone()
        }));
    }
    let error = call.error.clone()?;
    if interactions.is_empty() {
        return Some(ToolOutput::Plain(error.into()));
    }
    Some(ToolOutput::Mcp(McpOutput {
        blocks: vec![McpBlock::Text { text: error }],
        is_error: true,
        interactions,
        ..McpOutput::default()
    }))
}

/// A helper row's line: the verdicts once it has answered, and the call as the snippet made it
/// until then or when it failed.
fn helper_summary(call: &CodeCall, helper: McpHelper) -> String {
    call.output
        .as_ref()
        .filter(|_| call.status == CodeCallStatus::Success)
        .and_then(|output| decisions::summary(helper, Some(&call.input), output))
        .unwrap_or_else(|| call.called_as.clone())
}

/// What the snippet printed and ended on, each held to `budget` lines, then how it failed. The
/// flag says the budget left something out.
pub(crate) fn tail(output: &CodeOutput, budget: usize, width: u16) -> (Vec<Block>, bool) {
    let theme = theme::current();
    let mut blocks = Vec::new();
    let mut truncated = false;
    let stdout = output.stdout.trim_end();
    let printed = (!stdout.is_empty()).then(|| plain_body(stdout, width));
    let result = output
        .result
        .as_deref()
        .map(|result| match pretty_json(result) {
            Some(pretty) => json_body(&pretty, width),
            None => plain_body(result, width),
        });
    for (label, body) in [(PRINTED_LABEL, printed), (RESULT_LABEL, result)] {
        let Some((mut lines, source)) = body else {
            continue;
        };
        blocks.push((vec![label_line(label, theme.accent)], None));
        let (shown, hidden) = body_window(lines.len(), budget);
        lines.truncate(shown);
        blocks.push((lines, Some(source)));
        if hidden > 0 {
            blocks.push((vec![truncation_line(hidden)], None));
            truncated = true;
        }
    }
    if let Some(failure) = failure(&output.outcome) {
        blocks.push((vec![label_line(&failure, theme.tool_error)], None));
    }
    if let Some(note) = &output.state_note {
        blocks.push((vec![label_line(note, theme.tool_dim)], None));
    }
    if !output.repairs.is_empty() {
        let repairs = format!("{REPAIRED}{}", output.repairs.join(REPAIR_SEPARATOR));
        blocks.push((vec![label_line(&repairs, theme.tool_dim)], None));
    }
    (blocks, truncated)
}

fn failure(outcome: &CodeOutcome) -> Option<String> {
    match outcome {
        CodeOutcome::Running | CodeOutcome::Completed => None,
        CodeOutcome::Raised { kind, message } => Some(format!("{kind}{KIND_SEPARATOR}{message}")),
        CodeOutcome::Rejected { message } => Some(format!("{DID_NOT_RUN}{message}")),
        CodeOutcome::Limited { message } => Some(format!("{STOPPED}{message}")),
        CodeOutcome::Unavailable { message } => Some(message.clone()),
        CodeOutcome::Cancelled => Some(CANCELLED.to_owned()),
    }
}

fn label_line(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(super::mcp_card::one_line(text), style))
}

#[cfg(test)]
mod tests {
    use caudra_agent::types::Answerer;
    use caudra_agent::{ElicitAction, McpInteraction};
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const SERVER: &str = "github";
    const QUESTION: &str = "Which repository?";
    const TOOL: &str = "list_issues";
    const REFUSAL: &str = "plan mode allows only read-only calls";
    const PRINTED: &str = "first\nsecond\nthird\nfourth\nlast";
    const WIDTH: u16 = 80;

    fn call(status: CodeCallStatus, error: Option<&str>) -> CodeCall {
        CodeCall {
            server: SERVER.to_owned(),
            tool: TOOL.to_owned(),
            input: json!({"owner": "o"}),
            status,
            output: Some(McpOutput::default()),
            error: error.map(str::to_owned),
            ..CodeCall::default()
        }
    }

    fn interaction() -> McpInteraction {
        McpInteraction {
            message: QUESTION.to_owned(),
            url: None,
            action: ElicitAction::Decline,
            by: Answerer::User,
            answers: Vec::new(),
        }
    }

    /// A row opens to the server's answer whenever the call stands on it, with the questions the
    /// server asked on the way, and to plain text only when there is nothing else to show.
    #[test_case(Some(false), false, false ; "an_answer_with_no_error")]
    #[test_case(Some(true), true, false ; "an_answer_flagged_is_error")]
    #[test_case(Some(false), true, true ; "an_answer_too_large_to_keep")]
    #[test_case(None, true, true ; "a_failure_that_never_reached_an_answer")]
    fn a_row_keeps_the_questions_its_server_asked(
        answer: Option<bool>,
        failed: bool,
        shows_error: bool,
    ) {
        let call = CodeCall {
            output: answer.map(|is_error| McpOutput {
                is_error,
                ..McpOutput::default()
            }),
            error: failed.then(|| REFUSAL.to_owned()),
            interactions: vec![interaction()],
            ..call(CodeCallStatus::Error, None)
        };
        let Some(ToolOutput::Mcp(shown)) = shown_output(&call) else {
            panic!("expected an MCP card");
        };
        assert_eq!(shown.interactions, [interaction()]);
        let text = matches!(&shown.blocks[..], [McpBlock::Text { text }] if text == REFUSAL);
        assert_eq!(text, shows_error);
    }

    fn texts(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .flat_map(|(lines, _)| lines)
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test_case(CodeCallStatus::Running, None, BatchToolStatus::Running, false ; "running")]
    #[test_case(CodeCallStatus::Success, None, BatchToolStatus::Success, false ; "success")]
    #[test_case(CodeCallStatus::Refused, Some(REFUSAL), BatchToolStatus::Error, true ; "refused")]
    fn a_call_reads_as_a_batch_child(
        status: CodeCallStatus,
        error: Option<&str>,
        expected: BatchToolStatus,
        refused: bool,
    ) {
        let [entry] = entries(&CodeOutput {
            calls: vec![call(status, error)],
            ..CodeOutput::default()
        })
        .try_into()
        .unwrap();
        assert_eq!(entry.tool, format!("{SERVER}.{TOOL}"));
        assert_eq!(
            entry.summary,
            format!("{MCP_SUMMARY_PREFIX}{SERVER}.{TOOL}")
        );
        assert_eq!(entry.status, expected);
        assert_eq!(entry.refused, refused);
        match error {
            Some(error) => assert_eq!(
                entry.output.map(|output| output.as_text()).as_deref(),
                Some(error)
            ),
            None => assert!(matches!(entry.output, Some(ToolOutput::Mcp(_)))),
        }
    }

    #[test]
    fn the_tail_holds_each_part_to_the_budget_and_names_the_failure() {
        let output = CodeOutput {
            stdout: PRINTED.to_owned(),
            result: Some("[1, 2]".to_owned()),
            outcome: CodeOutcome::Raised {
                kind: "RuntimeError".to_owned(),
                message: "boom".to_owned(),
            },
            ..CodeOutput::default()
        };
        let (blocks, truncated) = tail(&output, 2, WIDTH);
        let texts = texts(&blocks);
        assert!(truncated);
        assert_eq!(texts[0], PRINTED_LABEL);
        assert_eq!(texts[1], "first");
        assert!(!texts.iter().any(|text| text == "last"), "{texts:?}");
        assert!(texts.iter().any(|text| text == RESULT_LABEL), "{texts:?}");
        assert_eq!(texts.last().map(String::as_str), Some("RuntimeError: boom"));
    }

    #[test]
    fn a_quiet_completed_run_has_no_tail() {
        let (blocks, truncated) = tail(&CodeOutput::default(), usize::MAX, WIDTH);
        assert!(blocks.is_empty() && !truncated);
    }

    #[test]
    fn the_tail_ends_on_what_was_repaired_before_the_run() {
        let output = CodeOutput {
            repairs: vec![
                "imported `asyncio`".to_owned(),
                "imported `json`".to_owned(),
            ],
            ..CodeOutput::default()
        };
        let (blocks, _) = tail(&output, usize::MAX, WIDTH);
        assert_eq!(
            texts(&blocks),
            [format!(
                "{REPAIRED}imported `asyncio`{REPAIR_SEPARATOR}imported `json`"
            )]
        );
    }
}
