//! What one `mcp` run did, as a card draws it live and restored. The model reads its own text,
//! rendered from the same run.

use caudra_storage::tool_ledger::ToolOutcome as LedgerOutcome;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::SEPARATOR;
use crate::mcp::output::{McpInteraction, McpOutput};
use crate::tools::ToolFailure;
use crate::types::counted;

/// The ledger source of a call a snippet made.
pub const CALL_SOURCE: &str = "mcp";

/// Calls a failed run lists for the model before summarising the rest.
const MAX_LISTED_CALLS: usize = 20;
const CALL_NOUN: &str = "call";
const PRINTED_LABEL: &str = "Printed:";
const RESULT_LABEL: &str = "Result:";
const NOTE_LABEL: &str = "Note:";
const REPAIRED_LABEL: &str = "Repaired before running:";
const LESSON_SEPARATOR: &str = "; ";
const NOTHING: &str = "The snippet ended on None and printed nothing.";
const CANCELLED: &str = "The user cancelled the run.";
const CALLS_LABEL: &str = "Server calls this run made before it stopped. Their effects stand, so check before repeating one:";
const SUCCEEDED: &str = "succeeded";
const FAILED: &str = "failed";
const REFUSED: &str = "refused";
const RUNNING: &str = "did not finish";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpCodeOutput {
    pub code: String,
    /// Every server call the snippet made, in the order it made them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<McpCodeCall>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    /// The value the snippet ended on, rendered for reading. `None` when it ended on `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub outcome: McpCodeOutcome,
    /// What became of the variables kept between runs, when something did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_note: Option<String>,
    /// What was changed in `code` before it ran, for the card. `code` stays as the model wrote it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<String>,
    /// What the model should write next time, for each repair that undid a slip of its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lessons: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpCodeOutcome {
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
}

/// One server call a snippet made.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpCodeCall {
    pub server: String,
    /// The tool's MCP name, or the helper's for a resource read.
    pub tool: String,
    /// The call as the snippet wrote it, `namespace.method` for a tool, so the model reads back
    /// the names it uses.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub called_as: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub input: Value,
    pub status: McpCodeCallStatus,
    /// What the server answered. Its questions to the user are in `interactions` instead.
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
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCodeCallStatus {
    #[default]
    Running,
    Success,
    /// The server failed it, or it never reached the server.
    Error,
    /// Denied, or kept from running by policy or the session's mode.
    Refused,
}

/// One call of a running `mcp` snippet changing state. The run's own card is the only row it has.
#[derive(Debug, Clone, Serialize)]
pub struct McpCodeProgressEvent {
    /// The `mcp` call's tool-use id.
    pub id: String,
    pub index: usize,
    pub call: McpCodeCall,
}

impl McpCodeCallStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

impl McpCodeOutcome {
    pub fn is_failure(&self) -> bool {
        self.failure().is_some()
    }

    /// How the run failed, as the tool ledger counts it, or `None` when it finished.
    pub fn failure(&self) -> Option<ToolFailure> {
        match self {
            Self::Running | Self::Completed => None,
            Self::Rejected { .. } => Some(ToolFailure::InvalidInput),
            Self::Cancelled => Some(ToolFailure::Cancelled),
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
        }
    }
}

impl McpCodeOutput {
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

    fn calls_made(&self) -> String {
        let mut lines = vec![CALLS_LABEL.to_owned()];
        lines.extend(
            self.calls
                .iter()
                .take(MAX_LISTED_CALLS)
                .map(McpCodeCall::line),
        );
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

impl McpCodeCall {
    /// The `server.tool` name permission rules, the ledger and cards know the call by.
    pub fn qualified_name(&self) -> String {
        format!("{}{SEPARATOR}{}", self.server, self.tool)
    }

    pub fn ledger_outcome(&self) -> LedgerOutcome {
        match (self.status, self.failure) {
            (McpCodeCallStatus::Success, _) => LedgerOutcome::Ok,
            (_, Some(failure)) => LedgerOutcome::from(failure),
            (McpCodeCallStatus::Refused, None) => LedgerOutcome::Denied,
            (McpCodeCallStatus::Running | McpCodeCallStatus::Error, None) => LedgerOutcome::Other,
        }
    }

    fn line(&self) -> String {
        let name = &self.called_as;
        let status = match self.status {
            McpCodeCallStatus::Running => RUNNING,
            McpCodeCallStatus::Success => SUCCEEDED,
            McpCodeCallStatus::Error => FAILED,
            McpCodeCallStatus::Refused => REFUSED,
        };
        match &self.error {
            Some(error) => format!("- {name} {status}: {error}"),
            None => format!("- {name} {status}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULT: &str = "5";
    const REPAIR: &str = "imported `asyncio`";
    const IMPORT_LESSON: &str = "`asyncio` was used without `import asyncio`";
    const RETURN_LESSON: &str = "end on the expression rather than `return` it";

    /// The model reads what to write next time, once each, and never the card's short form.
    #[test]
    fn the_model_reads_the_lesson_of_each_repair() {
        let output = McpCodeOutput {
            result: Some(RESULT.to_owned()),
            outcome: McpCodeOutcome::Completed,
            repairs: vec![REPAIR.to_owned()],
            lessons: vec![IMPORT_LESSON.to_owned(), RETURN_LESSON.to_owned()],
            ..McpCodeOutput::default()
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
}
