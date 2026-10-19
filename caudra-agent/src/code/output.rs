//! What one `code` run did, as a card draws it live and restored. The model reads its own text,
//! rendered from the same run.

use caudra_storage::tool_ledger::ToolOutcome as LedgerOutcome;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::SEPARATOR;
use crate::mcp::output::{McpHelper, McpInteraction, McpOutput};
use crate::tools::{ToolEffect, ToolFailure};
use crate::types::{BatchToolStatus, ToolInput, ToolOutput, counted};

/// The ledger source of a call a snippet made.
pub const CALL_SOURCE: &str = "code";

/// Calls a failed run lists for the model before summarising the rest.
const MAX_LISTED_CALLS: usize = 20;
const CALL_NOUN: &str = "call";
const PRINTED_LABEL: &str = "Printed:";
const RESULT_LABEL: &str = "Result:";
const NOTE_LABEL: &str = "Note:";
const REPAIRED_LABEL: &str = "Repaired:";
const LESSON_SEPARATOR: &str = "; ";
const AS_REPAIRED_LABEL: &str = "The lines as repaired:";
const LINE_NUMBER_WIDTH: usize = 4;
const LINE_BREAK: char = '\n';
/// Repaired lines the model reads before the rest are only counted.
const MAX_REPAIRED_LINES: usize = 20;
const MORE_REPAIRED_LEAD: &str = "… and ";
const MORE_REPAIRED_TAIL: &str = " more";
const NOTHING: &str = "The snippet ended on None and printed nothing.";
const CANCELLED: &str = "The user cancelled the run.";
const CALLS_LABEL: &str =
    "Calls this run made before it stopped. Their effects stand, so check before repeating one:";
const SUCCEEDED: &str = "succeeded";
const FAILED: &str = "failed";
const REFUSED: &str = "refused";
const RUNNING: &str = "did not finish";
const ARGUMENT_SEPARATOR: &str = ", ";
const COMMENT: char = '#';
const FENCE: &str = "```";
const ELLIPSIS: char = '…';
const SUMMARY_SEPARATOR: &str = " · ";
const LINE_NOUN: &str = "line";
/// How much of a longer snippet's first statement its row shows, so the line count after it
/// survives the one-row clamp.
const MAX_SNIPPET_PREVIEW_CHARS: usize = 60;

/// `history.read(ref="abc", limit=5000)`: a call as the snippet made it, with the values it ran
/// with, which a permission prompt and a subagent's row both spell this way.
pub fn written_call(name: &str, input: &Value) -> String {
    format!("{name}({})", call_arguments(input))
}

fn call_arguments(input: &Value) -> String {
    match input {
        Value::Object(arguments) => arguments
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(ARGUMENT_SEPARATOR),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

/// What a `code` run that has made no host calls is doing: a one-line snippet is its line, a
/// longer one its first statement and how long it is.
pub fn snippet_summary(code: &str) -> String {
    let count = code.lines().count();
    let first = code
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with(COMMENT) && !line.starts_with(FENCE));
    match (first, count) {
        (Some(line), 1) => line.to_owned(),
        (Some(line), _) => format!(
            "{}{SUMMARY_SEPARATOR}{}",
            preview(line),
            counted(count, LINE_NOUN)
        ),
        (None, _) => counted(count, LINE_NOUN),
    }
}

fn preview(line: &str) -> String {
    match line.char_indices().nth(MAX_SNIPPET_PREVIEW_CHARS) {
        Some((end, _)) => format!("{}{ELLIPSIS}", line[..end].trim_end()),
        None => line.to_owned(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeOutput {
    pub code: String,
    /// Every host call the snippet made, in the order it made them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<CodeCall>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    /// The value the snippet ended on, rendered for reading. `None` when it ended on `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub outcome: CodeOutcome,
    /// What became of the variables kept between runs, when something did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_note: Option<String>,
    /// What was changed in `code` before it ran, for the card. `code` stays as the model wrote it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<String>,
    /// What the model should write next time, for each repair that undid a slip of its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lessons: Vec<String>,
    /// The lines those repairs changed, as they ran, so the model sees the code it should have
    /// written beside the lessons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repaired_lines: Vec<RepairedLine>,
    /// The calls whose answers `stdout` only repeats, in the order printed, so the card can show
    /// their cards in its place. Empty when it printed anything else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub printed_repeats: Vec<usize>,
    /// The call whose answer the snippet ended on, when it ended on nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_repeats: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairedLine {
    /// 1-based, as the model wrote the snippet.
    pub line: usize,
    pub text: String,
}

/// The lines of `taught` that differ from `written`, which a repair keeps line for line; a line a
/// repair blanked, such as a Markdown fence, has nothing to teach.
pub fn repaired_lines(written: &str, taught: &str) -> Vec<RepairedLine> {
    if written.split(LINE_BREAK).count() != taught.split(LINE_BREAK).count() {
        return Vec::new();
    }
    written
        .split(LINE_BREAK)
        .zip(taught.split(LINE_BREAK))
        .enumerate()
        .filter(|(_, (written, taught))| written != taught && !taught.trim().is_empty())
        .map(|(index, (_, taught))| RepairedLine {
            line: index + 1,
            text: taught.to_owned(),
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CodeOutcome {
    #[default]
    Running,
    Completed,
    /// The snippet raised and did not catch it.
    Raised {
        kind: String,
        message: String,
    },
    /// The snippet never ran: it did not parse or type-check.
    Rejected {
        message: String,
    },
    /// A time, memory or call budget stopped it.
    Limited {
        message: String,
    },
    /// Nothing could run it.
    Unavailable {
        message: String,
    },
    Cancelled,
    /// It ran past its `timeoutSec`.
    TimedOut {
        after_secs: u64,
    },
}

/// One host call a snippet made: a server's tool, a resource read, or a decision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeCall {
    /// The MCP server, or the module the call reached, such as `decisions`.
    pub server: String,
    /// The tool's MCP name, or the helper's for any other call.
    pub tool: String,
    /// The call as the snippet wrote it, `namespace.method` for a tool, so the model reads back
    /// the names it uses.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub called_as: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub input: Value,
    pub status: CodeCallStatus,
    /// What the call answered. Its questions to the user are in `interactions` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<McpOutput>,
    /// Why the call failed or was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How the tool ledger counts a call that did not succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ToolFailure>,
    /// From the permission check to the answer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration_ms: u64,
    /// What the server asked the user during the call, whether or not it answered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interactions: Vec<McpInteraction>,
    /// The helper this call was, when it was not a server's tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<McpHelper>,
    /// The registered tool a module ran for this call, and what it answered as its own card
    /// would show it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted: Option<Box<HostedResult>>,
}

/// A hosted tool's own card on a `code` call's row: what it shows from the moment the tool starts,
/// then what it answered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostedResult {
    /// The tool's registered name, which the ledger counts the call under.
    pub tool: String,
    /// Where the tool came from, as the ledger records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub effect: ToolEffect,
    /// The body the tool's card draws while it runs, such as an image's prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ToolInput>,
    /// `None` until the tool answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ToolOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
}

impl HostedResult {
    /// A tool that has not started yet: its card is its name and header.
    pub fn pending(tool: &str) -> Self {
        Self {
            tool: tool.to_owned(),
            ..Self::default()
        }
    }
}

/// `ToolOutput` has no equality of its own, so outputs compare by what they serialize to.
impl PartialEq for HostedResult {
    fn eq(&self, other: &Self) -> bool {
        (
            &self.tool,
            &self.source,
            self.effect,
            &self.input,
            &self.annotation,
        ) == (
            &other.tool,
            &other.source,
            other.effect,
            &other.input,
            &other.annotation,
        ) && serde_json::to_value(&self.output).ok() == serde_json::to_value(&other.output).ok()
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeCallStatus {
    #[default]
    Running,
    Success,
    /// The server failed it, or it never reached the server.
    Error,
    /// Denied, or kept from running by policy or the session's mode.
    Refused,
}

/// One call of a running `code` snippet changing state. The run's own card is the only row it has.
#[derive(Debug, Clone, Serialize)]
pub struct CodeProgressEvent {
    /// The `code` call's tool-use id.
    pub id: String,
    pub index: usize,
    pub call: CodeCall,
}

impl CodeCallStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// A call's row reads like a batch child's, and a refusal is a failure there.
impl From<CodeCallStatus> for BatchToolStatus {
    fn from(status: CodeCallStatus) -> Self {
        match status {
            CodeCallStatus::Running => Self::Running,
            CodeCallStatus::Success => Self::Success,
            CodeCallStatus::Error | CodeCallStatus::Refused => Self::Error,
        }
    }
}

impl CodeOutcome {
    pub fn is_failure(&self) -> bool {
        self.failure().is_some()
    }

    /// How the run failed, as the tool ledger counts it, or `None` when it finished.
    pub fn failure(&self) -> Option<ToolFailure> {
        match self {
            Self::Running | Self::Completed => None,
            Self::Rejected { .. } => Some(ToolFailure::InvalidInput),
            Self::Cancelled => Some(ToolFailure::Cancelled),
            Self::TimedOut { .. } => Some(ToolFailure::Timeout),
            Self::Raised { .. } | Self::Limited { .. } | Self::Unavailable { .. } => {
                Some(ToolFailure::Other)
            }
        }
    }

    /// Why the run failed, as the model reads it.
    fn describe(&self) -> Option<String> {
        match self {
            Self::Running | Self::Completed => None,
            Self::Raised { kind, message } => Some(format!("The snippet raised {kind}: {message}")),
            Self::Rejected { message } => Some(format!("The snippet did not run: {message}")),
            Self::Limited { message } => Some(format!("The snippet was stopped: {message}")),
            Self::Unavailable { message } => Some(message.clone()),
            Self::Cancelled => Some(CANCELLED.to_owned()),
            Self::TimedOut { after_secs } => Some(format!(
                "The snippet ran past its timeoutSec of {after_secs}s and was stopped. Its output and assignments are lost, and the variables from before it are kept; pass a larger timeoutSec if it needs longer."
            )),
        }
    }
}

impl CodeOutput {
    /// Whether the printed output or the ending value only repeats this call's answer, so its
    /// card stands in for them.
    pub fn stands_in(&self, call: usize) -> bool {
        self.printed_repeats.contains(&call) || self.result_repeats == Some(call)
    }

    /// What the model reads: printed output, the result, and for a failed run why, with the
    /// calls already made so their side effects are not blindly repeated.
    pub fn model_text(&self) -> String {
        let mut parts = Vec::new();
        if !self.stdout.trim().is_empty() {
            parts.push(format!("{PRINTED_LABEL}\n{}", self.stdout.trim_end()));
        }
        if let Some(result) = &self.result {
            parts.push(format!("{RESULT_LABEL}\n{result}"));
        }
        if let Some(failure) = self.outcome.describe() {
            parts.push(failure);
            if !self.calls.is_empty() {
                parts.push(self.calls_made());
            }
        }
        if !self.lessons.is_empty() {
            parts.push(format!(
                "{REPAIRED_LABEL} {}.",
                self.lessons.join(LESSON_SEPARATOR)
            ));
        }
        if !self.repaired_lines.is_empty() {
            parts.push(self.lines_as_repaired());
        }
        if let Some(note) = &self.state_note {
            parts.push(format!("{NOTE_LABEL} {note}"));
        }
        if parts.is_empty() {
            return NOTHING.to_owned();
        }
        parts.join("\n\n")
    }

    pub fn annotation(&self) -> Option<String> {
        (!self.calls.is_empty()).then(|| counted(self.calls.len(), CALL_NOUN))
    }

    fn lines_as_repaired(&self) -> String {
        let mut lines = vec![AS_REPAIRED_LABEL.to_owned()];
        lines.extend(
            self.repaired_lines
                .iter()
                .take(MAX_REPAIRED_LINES)
                .map(|repaired| {
                    format!("{:>LINE_NUMBER_WIDTH$} | {}", repaired.line, repaired.text)
                }),
        );
        if let Some(rest) = self
            .repaired_lines
            .len()
            .checked_sub(MAX_REPAIRED_LINES)
            .filter(|rest| *rest > 0)
        {
            lines.push(format!("{MORE_REPAIRED_LEAD}{rest}{MORE_REPAIRED_TAIL}"));
        }
        lines.join("\n")
    }

    fn calls_made(&self) -> String {
        let mut lines = vec![CALLS_LABEL.to_owned()];
        lines.extend(self.calls.iter().take(MAX_LISTED_CALLS).map(CodeCall::line));
        if let Some(rest) = self
            .calls
            .len()
            .checked_sub(MAX_LISTED_CALLS)
            .filter(|rest| *rest > 0)
        {
            lines.push(format!("- and {rest} more"));
        }
        lines.join("\n")
    }
}

impl CodeCall {
    /// The `server.tool` name permission rules, the ledger and cards know the call by.
    pub fn qualified_name(&self) -> String {
        format!("{}{SEPARATOR}{}", self.server, self.tool)
    }

    /// The name the snippet called, else the qualified one.
    pub fn written_name(&self) -> String {
        match self.called_as.is_empty() {
            true => self.qualified_name(),
            false => self.called_as.clone(),
        }
    }

    /// `(key=value, …)`, what follows [`Self::written_name`] when the call is spelled out.
    pub fn written_arguments(&self) -> String {
        format!("({})", call_arguments(&self.input))
    }

    /// The tool and source the ledger counts the call under: a hosted tool's own, as when the
    /// model called it, else the module call's.
    pub fn ledger_name(&self) -> (String, String) {
        match &self.hosted {
            Some(hosted) => (
                hosted.tool.clone(),
                hosted
                    .source
                    .clone()
                    .unwrap_or_else(|| CALL_SOURCE.to_owned()),
            ),
            None => (self.qualified_name(), CALL_SOURCE.to_owned()),
        }
    }

    pub fn ledger_outcome(&self) -> LedgerOutcome {
        match (self.status, self.failure) {
            (CodeCallStatus::Success, _) => LedgerOutcome::Ok,
            (_, Some(failure)) => LedgerOutcome::from(failure),
            (CodeCallStatus::Refused, None) => LedgerOutcome::Denied,
            (CodeCallStatus::Running | CodeCallStatus::Error, None) => LedgerOutcome::Other,
        }
    }

    fn line(&self) -> String {
        let name = &self.called_as;
        let status = match self.status {
            CodeCallStatus::Running => RUNNING,
            CodeCallStatus::Success => SUCCEEDED,
            CodeCallStatus::Error => FAILED,
            CodeCallStatus::Refused => REFUSED,
        };
        match &self.error {
            Some(error) => format!("- {name} {status}: {error}"),
            None => format!("- {name} {status}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::mcp::output::{DecisionHelper, HistoryHelper};

    const RESULT: &str = "5";
    const REPAIR: &str = "imported `asyncio`";
    const IMPORT_LESSON: &str = "`asyncio` was used without `import asyncio`";
    const RETURN_LESSON: &str = "end on the expression rather than `return` it";

    /// The model reads what to write next time, once each, and never the card's short form.
    #[test]
    fn the_model_reads_the_lesson_of_each_repair() {
        let output = CodeOutput {
            result: Some(RESULT.to_owned()),
            outcome: CodeOutcome::Completed,
            repairs: vec![REPAIR.to_owned()],
            lessons: vec![IMPORT_LESSON.to_owned(), RETURN_LESSON.to_owned()],
            ..CodeOutput::default()
        };
        let text = output.model_text();
        assert!(
            text.ends_with(&format!(
                "{REPAIRED_LABEL} {IMPORT_LESSON}{LESSON_SEPARATOR}{RETURN_LESSON}."
            )),
            "{text}"
        );
        assert!(!text.contains(REPAIR), "{text}");
    }

    const WRITTEN: &str = "```python\nconst r = await gh.list({state: \"open\"}); r\nx = 1\n```";
    const TAUGHT: &str = "\nr = await gh.list(state=\"open\"); r\nx = 1\n";
    const TAUGHT_LINE: &str = "r = await gh.list(state=\"open\"); r";

    /// Only the lines a repair rewrote reach the model, numbered as it wrote them; a blanked fence
    /// line has nothing to teach.
    #[test]
    fn the_model_reads_the_lines_a_repair_rewrote() {
        let output = CodeOutput {
            outcome: CodeOutcome::Completed,
            lessons: vec![IMPORT_LESSON.to_owned()],
            repaired_lines: repaired_lines(WRITTEN, TAUGHT),
            ..CodeOutput::default()
        };
        let text = output.model_text();
        assert!(
            text.contains(&format!(
                "{AS_REPAIRED_LABEL}\n{:>LINE_NUMBER_WIDTH$} | {TAUGHT_LINE}",
                2
            )),
            "{text}"
        );
        assert_eq!(output.repaired_lines.len(), 1, "{text}");
    }

    #[test]
    fn lines_that_moved_teach_nothing() {
        assert!(repaired_lines(WRITTEN, TAUGHT_LINE).is_empty());
    }

    const SEARCH: &str = "history.search";

    #[test_case("search", json!({"query": "flaky", "limit": 5}), "history.search(query=\"flaky\", limit=5)" ; "keywords")]
    #[test_case("search", json!(null), "history.search()" ; "no_arguments")]
    #[test_case("", json!({}), "history.search()" ; "named_by_server_and_tool")]
    #[test_case("search", json!(["a"]), "history.search([\"a\"])" ; "not_an_object")]
    fn a_call_is_written_as_the_snippet_made_it(called_as: &str, input: Value, expected: &str) {
        let call = CodeCall {
            server: "history".to_owned(),
            tool: "search".to_owned(),
            called_as: match called_as {
                "" => String::new(),
                _ => SEARCH.to_owned(),
            },
            input,
            ..CodeCall::default()
        };
        assert_eq!(
            format!("{}{}", call.written_name(), call.written_arguments()),
            expected
        );
        assert_eq!(written_call(&call.written_name(), &call.input), expected);
    }

    const LONG_STATEMENT: &str = "rows = sorted(records, key=lambda record: (record['when'], record['who'], record['what']))";

    #[test_case("sum(x for x in rows)", "sum(x for x in rows)" ; "one_line")]
    #[test_case("rows = load()\nlen(rows)", "rows = load() · 2 lines" ; "several_lines")]
    #[test_case("# tally\n\nrows = load()\nlen(rows)", "rows = load() · 4 lines" ; "after_comments_and_blanks")]
    #[test_case("```python\nrows = load()\n```", "rows = load() · 3 lines" ; "inside_a_fence")]
    #[test_case("# only\n# comments", "2 lines" ; "nothing_but_comments")]
    fn a_snippet_without_calls_reads_as_its_first_statement(code: &str, expected: &str) {
        assert_eq!(snippet_summary(code), expected);
    }

    #[test]
    fn a_long_first_statement_is_cut_before_the_line_count() {
        let summary = snippet_summary(&format!("{LONG_STATEMENT}\nrows"));
        let (statement, count) = summary.split_once(SUMMARY_SEPARATOR).unwrap();
        assert_eq!(statement.chars().count(), MAX_SNIPPET_PREVIEW_CHARS + 1);
        assert!(statement.ends_with(ELLIPSIS), "{summary}");
        assert_eq!(count, counted(2, LINE_NOUN));
    }

    /// Calls saved before helpers were marked load as server calls, and a marked one loads as it
    /// was.
    #[test_case(json!({"server": "s", "tool": "t", "input": {}, "status": "success"}), None ; "saved_before_helpers")]
    #[test_case(json!({"server": "s", "tool": "t", "input": {}, "status": "success", "helper": "rerank"}), Some(McpHelper::Decision(DecisionHelper::Rerank)) ; "rerank")]
    #[test_case(json!({"server": "s", "tool": "t", "input": {}, "status": "success", "helper": "decide"}), Some(McpHelper::Decision(DecisionHelper::Decide)) ; "decide")]
    #[test_case(json!({"server": "s", "tool": "t", "input": {}, "status": "success", "helper": "sessions"}), Some(McpHelper::History(HistoryHelper::Sessions)) ; "history")]
    fn a_stored_call_keeps_its_helper(stored: Value, helper: Option<McpHelper>) {
        let call: CodeCall = serde_json::from_value(stored).unwrap();
        assert_eq!(call.helper, helper);
        let again: CodeCall = serde_json::from_value(serde_json::to_value(&call).unwrap()).unwrap();
        assert_eq!(again, call);
    }
}
