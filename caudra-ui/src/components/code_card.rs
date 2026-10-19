//! The `code` tool's card under its snippet: a row for each server call the snippet made, which
//! opens to that call's own MCP card, then what the snippet printed, the value it ended on, and
//! why it stopped when it did not finish.

use std::ops::Range;

use caudra_agent::code::MODULE_DOCS;
use caudra_agent::code::output::{CodeCall, CodeCallStatus, CodeOutcome, CodeOutput};
use caudra_agent::tools::ToolEffect;
use caudra_agent::{
    BatchToolEntry, BatchToolStatus, DecisionHelper, MCP_SUMMARY_PREFIX, McpBlock, McpHelper,
    McpOutput, ToolOutput, pretty_json,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::code_view::{
    BodySource, TreeFolds, body_window, json_body, plain_body, tree_body, truncation_line,
};
use super::literal;
use super::mcp_card::decisions;
use crate::theme;

const PRINTED_LABEL: &str = "Printed";
const RESULT_LABEL: &str = "Result";
const DID_NOT_RUN: &str = "Did not run: ";
const STOPPED: &str = "Stopped: ";
const CANCELLED: &str = "Cancelled";
const TIMED_OUT: &str = "Timed out after ";
const KIND_SEPARATOR: &str = ": ";
const REPAIRED: &str = "Repaired: ";
const REPAIR_SEPARATOR: &str = ", ";
const LINE_BREAK: char = '\n';

/// A run of the card's lines, the text a selection over them copies (`None` is chrome), and for
/// each line the tree and node a click on it folds or opens.
pub(crate) struct Block {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) source: Option<BodySource>,
    pub(crate) nodes: Vec<Option<(usize, usize)>>,
}

impl Block {
    fn chrome(line: Line<'static>) -> Self {
        Self {
            lines: vec![line],
            source: None,
            nodes: vec![None],
        }
    }

    fn text((lines, source): (Vec<Line<'static>>, BodySource)) -> Self {
        Self {
            nodes: vec![None; lines.len()],
            lines,
            source: Some(source),
        }
    }

    /// A tree's first row is always its root, which a click folds.
    fn is_tree(&self) -> bool {
        self.nodes.first().is_some_and(Option::is_some)
    }

    /// The lines in `kept`, with the part of the source they came from.
    pub(crate) fn keep(&self, kept: Range<usize>) -> Self {
        Self {
            lines: self.lines[kept.clone()].to_vec(),
            source: self
                .source
                .clone()
                .and_then(|source| source.keep_rows(kept.clone())),
            nodes: self.nodes[kept].to_vec(),
        }
    }
}

/// Each call as a batch child, so its row reads, folds and opens as a direct MCP call's card
/// would.
pub(crate) fn entries(output: &CodeOutput) -> Vec<BatchToolEntry> {
    output.calls.iter().map(entry).collect()
}

fn entry(call: &CodeCall) -> BatchToolEntry {
    let tool = call.qualified_name();
    let status = BatchToolStatus::from(call.status);
    let refused = call.status == CodeCallStatus::Refused;
    if let Some(hosted) = &call.hosted {
        return BatchToolEntry {
            summary: call.title.clone().unwrap_or_else(|| call.called_as.clone()),
            tool: hosted.tool.clone(),
            effect: hosted.effect,
            status,
            input: hosted.input.clone(),
            raw_input: Some(call.input.clone()),
            annotation: hosted.annotation.clone(),
            output: hosted.output.clone().or_else(|| {
                call.error
                    .clone()
                    .map(|error| ToolOutput::Plain(error.into()))
            }),
            model_suffix: None,
            refused,
            accounting: Default::default(),
        };
    }
    let output = shown_output(call);
    let module_call = MODULE_DOCS.iter().any(|doc| doc.namespace == call.server);
    let summary = match call.helper {
        Some(McpHelper::Decision(helper)) => decision_summary(call, helper),
        _ if module_call => call.title.clone().unwrap_or_else(|| call.called_as.clone()),
        _ => format!("{MCP_SUMMARY_PREFIX}{tool}"),
    };
    BatchToolEntry {
        summary,
        tool,
        effect: ToolEffect::Unknown,
        status,
        input: None,
        raw_input: Some(call.input.clone()),
        annotation: match (call.helper, &output) {
            (Some(McpHelper::History(_)), Some(ToolOutput::Mcp(answer))) => history_summary(answer),
            (Some(McpHelper::History(_)), _) => None,
            (_, output) => output.as_ref().and_then(ToolOutput::annotation),
        },
        output,
        model_suffix: None,
        refused,
        accounting: Default::default(),
    }
}

/// What a history call found, in the line it answered with: how a search ranked its hits, which
/// session a read came from, how many sessions a listing holds. A row folds a history card, so
/// this is what it says in its place, where a count of the card's lines would say nothing.
fn history_summary(answer: &McpOutput) -> Option<String> {
    if answer.is_error {
        return None;
    }
    answer.blocks.iter().find_map(|block| match block {
        McpBlock::Text { text } => Some(text.clone()),
        _ => None,
    })
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

/// A decision row's line: the verdicts once it has answered, and the call as the snippet made it
/// until then or when it failed.
fn decision_summary(call: &CodeCall, helper: DecisionHelper) -> String {
    call.output
        .as_ref()
        .filter(|_| call.status == CodeCallStatus::Success)
        .and_then(|output| decisions::summary(helper, Some(&call.input), output))
        .unwrap_or_else(|| call.called_as.clone())
}

/// What the snippet printed and ended on, read once, so the card can ask whether it holds a tree
/// before it decides how to draw it.
pub(crate) struct Tail<'a> {
    output: &'a CodeOutput,
    printed: Option<Vec<Piece<'a>>>,
    result: Option<Vec<Piece<'a>>>,
}

impl<'a> Tail<'a> {
    pub(crate) fn new(output: &'a CodeOutput) -> Self {
        let stdout = output.stdout.trim_end();
        Self {
            output,
            printed: (!stdout.is_empty()).then(|| printed_pieces(stdout)),
            result: output
                .result
                .as_deref()
                .map(|result| match literal::tree(result) {
                    Some(value) => vec![Piece::Tree(value)],
                    None => vec![Piece::Result(result)],
                }),
        }
    }

    /// A tree folds by itself, so a tail holding one is drawn whole rather than in a window.
    pub(crate) fn has_tree(&self) -> bool {
        [&self.printed, &self.result]
            .into_iter()
            .flatten()
            .flatten()
            .any(|piece| matches!(piece, Piece::Tree(_)))
    }
}

/// What the snippet printed and ended on, the text of each held to `budget` lines, then how it
/// failed. The flag says the budget left something out.
///
/// A dict or a list printed on its own, or ended on, is a tree that rests as its root row, which
/// opens whole, and `folds` says which nodes a click moved, each tree numbered in the order it
/// draws. A tree is never cut short: what it hides is the reader's to fold.
pub(crate) fn tail(
    tail: Tail<'_>,
    budget: usize,
    width: u16,
    folds: &TreeFolds,
) -> (Vec<Block>, bool) {
    let Tail {
        output,
        printed,
        result,
    } = tail;
    let theme = theme::current();
    let mut blocks = Vec::new();
    let mut truncated = false;
    let mut trees = 0;
    for (label, pieces) in [(PRINTED_LABEL, printed), (RESULT_LABEL, result)] {
        let Some(pieces) = pieces else {
            continue;
        };
        blocks.push(Block::chrome(label_line(label, theme.accent)));
        let mut body = Vec::new();
        for piece in pieces {
            match piece {
                Piece::Text(text) => body.push(Block::text(plain_body(text, width))),
                Piece::Result(result) => body.push(Block::text(match pretty_json(result) {
                    Some(pretty) => json_body(&pretty, width),
                    None => plain_body(result, width),
                })),
                Piece::Tree(value) => {
                    let folded = folds.folded(trees, true);
                    if let Some((lines, source, nodes)) = tree_body(&value, &folded, width) {
                        let nodes = nodes
                            .into_iter()
                            .map(|node| node.map(|node| (trees, node)))
                            .collect();
                        body.push(Block {
                            lines,
                            source: Some(source),
                            nodes,
                        });
                    }
                    trees += 1;
                }
            }
        }
        let hidden = hold(&mut body, budget);
        blocks.extend(body);
        if hidden > 0 {
            blocks.push(Block::chrome(truncation_line(hidden)));
            truncated = true;
        }
    }
    if let Some(failure) = failure(&output.outcome) {
        blocks.push(Block::chrome(label_line(&failure, theme.tool_error)));
    }
    if let Some(note) = &output.state_note {
        blocks.push(Block::chrome(label_line(note, theme.tool_dim)));
    }
    if !output.repairs.is_empty() {
        let repairs = format!("{REPAIRED}{}", output.repairs.join(REPAIR_SEPARATOR));
        blocks.push(Block::chrome(label_line(&repairs, theme.tool_dim)));
    }
    (blocks, truncated)
}

/// A part of what the snippet printed or ended on, as it draws.
enum Piece<'a> {
    Text(&'a str),
    Result(&'a str),
    Tree(Value),
}

/// `stdout` as one tree when it is all one value, else a tree for each line that is one, between
/// runs of text, as a loop printing several answers writes them.
fn printed_pieces(stdout: &str) -> Vec<Piece<'_>> {
    if let Some(value) = literal::tree(stdout) {
        return vec![Piece::Tree(value)];
    }
    let mut pieces = Vec::new();
    let mut text_start = 0;
    let mut at = 0;
    for line in stdout.split_inclusive(LINE_BREAK) {
        let next = at + line.len();
        if let Some(value) = literal::tree(line) {
            if text_start < at {
                pieces.push(Piece::Text(
                    stdout[text_start..at].trim_end_matches(LINE_BREAK),
                ));
            }
            pieces.push(Piece::Tree(value));
            text_start = next;
        }
        at = next;
    }
    if text_start < stdout.len() {
        pieces.push(Piece::Text(&stdout[text_start..]));
    }
    pieces
}

/// Keeps the first rows of text in `body` that fit `budget`, and every tree whole, and says how
/// many rows it dropped.
fn hold(body: &mut Vec<Block>, budget: usize) -> usize {
    let total: usize = body
        .iter()
        .filter(|block| !block.is_tree())
        .map(|block| block.lines.len())
        .sum();
    let (mut room, hidden) = body_window(total, budget);
    if hidden == 0 {
        return 0;
    }
    body.retain_mut(|block| {
        if block.is_tree() {
            return true;
        }
        let kept = block.lines.len().min(room);
        room -= kept;
        *block = block.keep(0..kept);
        kept > 0
    });
    hidden
}

fn failure(outcome: &CodeOutcome) -> Option<String> {
    match outcome {
        CodeOutcome::Running | CodeOutcome::Completed => None,
        CodeOutcome::Raised { kind, message } => Some(format!("{kind}{KIND_SEPARATOR}{message}")),
        CodeOutcome::Rejected { message } => Some(format!("{DID_NOT_RUN}{message}")),
        CodeOutcome::Limited { message } => Some(format!("{STOPPED}{message}")),
        CodeOutcome::Unavailable { message } => Some(message.clone()),
        CodeOutcome::Cancelled => Some(CANCELLED.to_owned()),
        CodeOutcome::TimedOut { after_secs } => Some(format!("{TIMED_OUT}{after_secs}s")),
    }
}

fn label_line(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(super::mcp_card::one_line(text), style))
}

#[cfg(test)]
pub(crate) mod tests {
    use caudra_agent::types::Answerer;
    use caudra_agent::{ElicitAction, HistoryHelper, McpInteraction};
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::components::code_view::BatchViews;

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
            .flat_map(|block| &block.lines)
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

    const HISTORY_CALL: &str = "history.sessions(\"project\")";
    const HISTORY_FOUND: &str = "1 session";

    pub(crate) fn history_row(is_error: bool) -> CodeCall {
        CodeCall {
            server: "history".to_owned(),
            tool: "sessions".to_owned(),
            called_as: HISTORY_CALL.to_owned(),
            helper: Some(McpHelper::History(HistoryHelper::Sessions)),
            output: Some(McpOutput {
                blocks: vec![McpBlock::Text {
                    text: HISTORY_FOUND.to_owned(),
                }],
                structured: Some(json!([{"title": "t", "updated": "", "items": 1}])),
                is_error,
                ..McpOutput::default()
            }),
            ..call(CodeCallStatus::Success, None)
        }
    }

    /// A history row says what it found in the line it answered with, in place of a line count
    /// that would say nothing, and opens to the whole answer. A failed call's row says nothing of
    /// a finding it never made.
    #[test_case(false, Some(HISTORY_FOUND) ; "found")]
    #[test_case(true, None ; "failed")]
    fn a_history_row_says_what_it_found(is_error: bool, expected: Option<&str>) {
        let [entry] = entries(&CodeOutput {
            calls: vec![history_row(is_error)],
            ..CodeOutput::default()
        })
        .try_into()
        .unwrap();

        assert_eq!(entry.summary, HISTORY_CALL);
        assert_eq!(entry.annotation.as_deref(), expected);
        let Some(ToolOutput::Mcp(shown)) = entry.output else {
            panic!("expected an MCP card");
        };
        assert_eq!(
            shown.helper,
            Some(McpHelper::History(HistoryHelper::Sessions))
        );
    }

    const HOSTED_TOOL: &str = "image_generate";
    const SAVED_PATH: &str = "fox.png";
    const HOSTED_HEADER: &str = "generate fox.png";

    /// A module call that ran a tool reads as that tool's own child row, restored from its saved
    /// card, so a thumbnail draws as it would on its own.
    #[test]
    fn a_hosted_call_reads_as_its_tools_card() {
        let stored = json!({
            "server": "image",
            "tool": "generate",
            "called_as": "image.generate",
            "title": HOSTED_HEADER,
            "input": {"prompt": "a fox", "out": SAVED_PATH},
            "status": "success",
            "hosted": {
                "tool": HOSTED_TOOL,
                "output": serde_json::to_value(ToolOutput::SavedImage {
                    path: SAVED_PATH.to_owned(),
                    text: String::new(),
                })
                .unwrap(),
            },
        });
        let call: CodeCall = serde_json::from_value(stored).unwrap();
        let [entry] = entries(&CodeOutput {
            calls: vec![call],
            ..CodeOutput::default()
        })
        .try_into()
        .unwrap();
        assert_eq!(entry.tool, HOSTED_TOOL);
        assert_eq!(entry.summary, HOSTED_HEADER);
        assert!(matches!(entry.output, Some(ToolOutput::SavedImage { .. })));
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
        let (blocks, truncated) = tail(Tail::new(&output), 2, WIDTH, &TreeFolds::default());
        let texts = texts(&blocks);
        assert!(truncated);
        assert_eq!(texts[0], PRINTED_LABEL);
        assert_eq!(texts[1], "first");
        assert!(!texts.iter().any(|text| text == "last"), "{texts:?}");
        assert!(texts.iter().any(|text| text == RESULT_LABEL), "{texts:?}");
        assert_eq!(texts.last().map(String::as_str), Some("RuntimeError: boom"));
    }

    pub(crate) const DUMP: &str = "{'title': 't'}";

    /// A run that printed only what its history call answered.
    pub(crate) fn repeating_run() -> CodeOutput {
        CodeOutput {
            calls: vec![history_row(false)],
            stdout: format!("{DUMP}\n"),
            printed_repeats: vec![0],
            ..CodeOutput::default()
        }
    }

    pub(crate) const DUMP_ROOT: &str = "\u{25b8} {\u{2026}} 1 key";
    pub(crate) const DUMP_FIELD: &str = "    \"title\": \"t\"";
    const DUMP_OPEN: &str = "\u{25be} {";
    const DUMP_CLOSE: &str = "  }";
    const LIST: &str = "[1]";
    const LIST_ROOT: &str = "\u{25b8} [\u{2026}] 1 item";

    fn printing(stdout: &str, result: Option<&str>) -> CodeOutput {
        CodeOutput {
            stdout: stdout.to_owned(),
            result: result.map(str::to_owned),
            ..CodeOutput::default()
        }
    }

    const NESTED: &str = "{'a': {'b': 1}}";
    const NESTED_OPEN: &str = "  \u{25be} \"a\": {";
    const NESTED_FOLDED: &str = "  \u{25b8} \"a\": {\u{2026}} 1 key";
    const NESTED_FIELD: &str = "      \"b\": 1";
    const NESTED_CLOSE: &str = "    }";
    const ROOT: (usize, usize) = (0, 0);
    const INNER: (usize, usize) = (0, 1);
    /// The child a tail's trees answer for in these tests.
    const TAIL: usize = 0;

    type Row = (String, Option<(usize, usize)>);

    /// What the reader clicked, as (tree, node), in order.
    fn clicked(clicks: &[(usize, usize)]) -> TreeFolds {
        clicks
            .iter()
            .fold(BatchViews::default(), |views, (tree, node)| {
                views.node_toggled(TAIL, *tree, *node)
            })
            .tree_folds(TAIL)
    }

    /// Each row's text and the node a click on it folds or opens.
    fn targets(blocks: &[Block]) -> Vec<Row> {
        blocks
            .iter()
            .flat_map(|block| {
                texts(std::slice::from_ref(block))
                    .into_iter()
                    .zip(block.nodes.iter().copied())
            })
            .collect()
    }

    fn row(text: &str, node: Option<(usize, usize)>) -> Row {
        (text.to_owned(), node)
    }

    /// A dict or a list rests as its root row, which a click opens whole and folds again; a line
    /// of it among text is a tree of its own, and the value the snippet ended on is another.
    #[test_case(printing(DUMP, None), &[] => vec![row(PRINTED_LABEL, None), row(DUMP_ROOT, Some(ROOT))] ; "a_printed_value_rests_folded")]
    #[test_case(printing(DUMP, None), &[ROOT] => vec![row(PRINTED_LABEL, None), row(DUMP_OPEN, Some(ROOT)), row(DUMP_FIELD, None), row(DUMP_CLOSE, None)] ; "a_click_opens_it_whole")]
    #[test_case(printing(&format!("found 1\n{DUMP}\ndone"), None), &[] => vec![row(PRINTED_LABEL, None), row("found 1", None), row(DUMP_ROOT, Some(ROOT)), row("done", None)] ; "a_line_among_text")]
    #[test_case(printing(DUMP, Some(LIST)), &[(1, 0)] => vec![row(PRINTED_LABEL, None), row(DUMP_ROOT, Some(ROOT)), row(RESULT_LABEL, None), row("\u{25be} [", Some((1, 0))), row("    1", None), row("  ]", None)] ; "the_result_is_the_next_tree")]
    #[test_case(printing("found 1", None), &[] => vec![row(PRINTED_LABEL, None), row("found 1", None)] ; "text_stays_text")]
    fn a_printed_value_is_a_tree(output: CodeOutput, clicks: &[(usize, usize)]) -> Vec<Row> {
        let (blocks, _) = tail(Tail::new(&output), usize::MAX, WIDTH, &clicked(clicks));
        targets(&blocks)
    }

    /// Under an open root every node folds and opens on its own, and folding the root forgets
    /// them, so opening it again opens all of it.
    #[test_case(&[ROOT] => vec![row(DUMP_OPEN, Some(ROOT)), row(NESTED_OPEN, Some(INNER)), row(NESTED_FIELD, None), row(NESTED_CLOSE, None), row(DUMP_CLOSE, None)] ; "opened_whole")]
    #[test_case(&[ROOT, INNER] => vec![row(DUMP_OPEN, Some(ROOT)), row(NESTED_FOLDED, Some(INNER)), row(DUMP_CLOSE, None)] ; "a_child_folds")]
    #[test_case(&[ROOT, INNER, INNER] => vec![row(DUMP_OPEN, Some(ROOT)), row(NESTED_OPEN, Some(INNER)), row(NESTED_FIELD, None), row(NESTED_CLOSE, None), row(DUMP_CLOSE, None)] ; "and_opens_again")]
    #[test_case(&[ROOT, INNER, ROOT, ROOT] => vec![row(DUMP_OPEN, Some(ROOT)), row(NESTED_OPEN, Some(INNER)), row(NESTED_FIELD, None), row(NESTED_CLOSE, None), row(DUMP_CLOSE, None)] ; "reopening_the_root_opens_all")]
    fn every_node_of_an_open_tree_folds(clicks: &[(usize, usize)]) -> Vec<Row> {
        let (blocks, _) = tail(
            Tail::new(&printing(NESTED, None)),
            usize::MAX,
            WIDTH,
            &clicked(clicks),
        );
        targets(&blocks[1..])
    }

    const TREE_CUT_SHORT: &str = "the budget holds back text, never part of a tree";

    #[test]
    fn a_tree_is_never_cut_short() {
        let output = printing(&format!("{PRINTED}\n{NESTED}"), None);

        let (blocks, truncated) = tail(Tail::new(&output), 2, WIDTH, &clicked(&[ROOT]));

        let texts = texts(&blocks);
        assert!(truncated, "{TREE_CUT_SHORT}");
        assert!(!texts.iter().any(|text| text == "last"), "{TREE_CUT_SHORT}");
        assert!(
            texts.iter().any(|text| text == NESTED_FIELD),
            "{TREE_CUT_SHORT}: {texts:?}"
        );
    }

    #[test]
    fn a_folded_list_says_how_much_it_holds() {
        let (blocks, _) = tail(
            Tail::new(&printing("", Some(LIST))),
            usize::MAX,
            WIDTH,
            &clicked(&[]),
        );
        assert_eq!(targets(&blocks)[1], row(LIST_ROOT, Some(ROOT)));
    }

    #[test]
    fn a_quiet_completed_run_has_no_tail() {
        let (blocks, truncated) = tail(
            Tail::new(&CodeOutput::default()),
            usize::MAX,
            WIDTH,
            &TreeFolds::default(),
        );
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
        let (blocks, _) = tail(Tail::new(&output), usize::MAX, WIDTH, &TreeFolds::default());
        assert_eq!(
            texts(&blocks),
            [format!(
                "{REPAIRED}imported `asyncio`{REPAIR_SEPARATOR}imported `json`"
            )]
        );
    }
}
