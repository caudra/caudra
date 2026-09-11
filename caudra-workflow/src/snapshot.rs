//! Runtime-neutral views of a workflow run: what the UI, the SDK, and the
//! journal agree a run looks like, independent of the script engine.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::journal::CallKind;

pub const WORKFLOW_LANGUAGE_VERSION: u32 = 1;
pub const WORKFLOW_ABI_VERSION: u32 = 1;
pub const DEFAULT_AGENT_BUDGET: u32 = 128;
pub const MAX_AGENT_BUDGET: u32 = 256;
pub const MAX_ACTIVE_RUNS: usize = 4;
pub const MAX_RUN_LOG_ENTRIES: usize = 200;
pub const MAX_PHASE_HISTORY: usize = 64;
/// How much of a call's result or error an inspection quotes.
pub const MAX_CALL_PREVIEW_BYTES: usize = 512;
/// How much of a call's request or result a body fetch carries. A body is
/// asked for one call at a time, so it can afford what a whole journal cannot.
pub const MAX_CALL_BODY_BYTES: usize = 64 * 1024;
const CALL_PREVIEW_MARKER: &str = "…";
/// The result field a script fills with the scratch file it wrote.
const RESULT_PATH_FIELD: &str = "path";

macro_rules! text_enum {
    ($name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    SourceKind {
        Builtin = "builtin",
        Project = "project",
        User = "user",
    }
}

text_enum! {
    RunStatus {
        Active = "active",
        Paused = "paused",
        BudgetLimited = "budget_limited",
        Interrupted = "interrupted",
        Completed = "completed",
        Cancelled = "cancelled",
        Failed = "failed",
    }
}

text_enum! {
    RosterState {
        Pending = "pending",
        Running = "running",
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
    }
}

text_enum! {
    RunEventKind {
        Phase = "phase",
        Log = "log",
    }
}

text_enum! {
    CallState {
        Started = "started",
        Completed = "completed",
        Failed = "failed",
    }
}

impl RunStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::Completed | Self::Cancelled | Self::Failed
        )
    }

    /// Whether the journal can be replayed into a new execution epoch. A
    /// budget-limited run resumes only under a higher budget, and an
    /// interrupted run never does.
    pub const fn is_resumable(self) -> bool {
        matches!(self, Self::Paused | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRosterEntry {
    pub call_key: u64,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub state: RosterState,
    pub tokens_used: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunUsage {
    pub agents_admitted: u32,
    pub tokens_used: u64,
}

/// A line the script logged, stamped in seconds since the epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    pub at: u64,
    pub message: String,
}

/// A phase the run entered, in seconds since the epoch. A phase ends when
/// the next one starts or the run settles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseRecord {
    pub title: String,
    pub started_at: u64,
}

/// One row of a run's stored timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub seq: u64,
    pub at: u64,
    pub kind: RunEventKind,
    pub text: String,
}

/// One journaled host call as an inspection shows it: what it was, how it
/// went, and a bounded quote of what it returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCall {
    pub call_key: u64,
    pub kind: CallKind,
    pub state: CallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The opening of what the script asked for, so a row says what it was
    /// told and not only what it was called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub tokens_used: u64,
    pub duration_ms: u64,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One call's request and result as they were journaled, cut only at
/// [`MAX_CALL_BODY_BYTES`]. This is what a reader opens when the preview on
/// the row is not enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCallBody {
    pub call_key: u64,
    pub request: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Everything known about one run: its snapshot, its journal, and its
/// timeline. `journal_trimmed` says the calls are gone although agents ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunDetail {
    pub run: RunSnapshot,
    #[serde(default)]
    pub calls: Vec<RunCall>,
    #[serde(default)]
    pub events: Vec<RunEvent>,
    #[serde(default)]
    pub journal_trimmed: bool,
}

/// A run of another session, named by the session that ran it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunHistoryEntry {
    pub run: RunSnapshot,
    pub session_id: String,
    pub session_title: String,
}

/// `text` cut to [`MAX_CALL_PREVIEW_BYTES`] on a character boundary.
pub fn call_preview(text: &str) -> String {
    cut(text, MAX_CALL_PREVIEW_BYTES)
}

/// `text` cut to [`MAX_CALL_BODY_BYTES`] on a character boundary.
pub fn call_body(text: &str) -> String {
    cut(text, MAX_CALL_BODY_BYTES)
}

fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(limit);
    format!("{}{CALL_PREVIEW_MARKER}", &text[..end])
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run_id: String,
    pub display_name: String,
    pub workflow_name: String,
    pub source_kind: SourceKind,
    /// Where the script the run executed lives, for a run that came from a
    /// file. A builtin is embedded and has nowhere to point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_message: Option<String>,
    pub revision: u64,
    pub execution_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
    #[serde(default)]
    pub phase_history: Vec<PhaseRecord>,
    pub agent_budget: u32,
    #[serde(default)]
    pub usage: RunUsage,
    #[serde(default)]
    pub roster: Vec<AgentRosterEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub logs: Vec<LogLine>,
    /// The latest state has not been delivered to whoever launched the run.
    #[serde(default)]
    pub outbox_pending: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

/// The read model a runtime publishes: every run of the session, newest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    #[serde(default)]
    pub runs: Vec<RunSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowEvent {
    Snapshot(Box<RunSnapshot>),
    Log {
        run_id: String,
        revision: u64,
        at: u64,
        message: String,
    },
}

impl RunSnapshot {
    /// Seconds the run has been going, or took: a settled run stops at its
    /// last update.
    pub fn elapsed_secs(&self, now: u64) -> u64 {
        let end = if self.status.is_terminal() {
            self.updated_at
        } else {
            now
        };
        end.saturating_sub(self.created_at)
    }

    /// Position of the current phase among the declared ones, 1-based, when
    /// the script declared phases and is in one of them.
    pub fn phase_position(&self) -> Option<(usize, usize)> {
        let phase = self.phase.as_deref()?;
        let index = self.phases.iter().position(|title| title == phase)?;
        Some((index + 1, self.phases.len()))
    }

    /// The scratch file the run's result names, once it has one.
    pub fn scratch_path(&self) -> Option<&str> {
        self.result.as_ref()?.get(RESULT_PATH_FIELD)?.as_str()
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const RUN_ID: &str = "run-1";
    const MESSAGE: &str = "phase started";

    #[test_case(RunStatus::Active => (false, false); "active")]
    #[test_case(RunStatus::Paused => (false, true); "paused")]
    #[test_case(RunStatus::BudgetLimited => (false, false); "budget_limited")]
    #[test_case(RunStatus::Interrupted => (true, false); "interrupted")]
    #[test_case(RunStatus::Completed => (true, false); "completed")]
    #[test_case(RunStatus::Cancelled => (true, true); "cancelled")]
    #[test_case(RunStatus::Failed => (true, true); "failed")]
    fn status_classification(status: RunStatus) -> (bool, bool) {
        (status.is_terminal(), status.is_resumable())
    }

    #[test_case(RunStatus::BudgetLimited, "budget_limited"; "run_status")]
    #[test_case(SourceKind::Builtin, "builtin"; "source_kind")]
    #[test_case(RosterState::Running, "running"; "roster_state")]
    fn display_matches_serde_text<T: fmt::Display + Serialize>(value: T, text: &str) {
        assert_eq!(value.to_string(), text);
        assert_eq!(serde_json::to_value(&value).unwrap(), text);
    }

    fn snapshot(status: RunStatus, phase: Option<&str>) -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_ID.into(),
            display_name: RUN_ID.into(),
            workflow_name: RUN_ID.into(),
            source_kind: SourceKind::Builtin,
            source_path: None,
            objective: None,
            status,
            pause_kind: None,
            pause_message: None,
            revision: 0,
            execution_epoch: 0,
            phase: phase.map(str::to_owned),
            phases: vec!["Plan".into(), "Research".into()],
            phase_history: Vec::new(),
            agent_budget: 1,
            usage: RunUsage::default(),
            roster: Vec::new(),
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: false,
            created_at: 100,
            updated_at: 130,
        }
    }

    #[test_case(RunStatus::Active, 200 => 100; "active_counts_to_now")]
    #[test_case(RunStatus::Completed, 200 => 30; "settled_stops_at_its_last_update")]
    #[test_case(RunStatus::Active, 50 => 0; "a_clock_behind_the_start_reads_zero")]
    fn elapsed_follows_the_status(status: RunStatus, now: u64) -> u64 {
        snapshot(status, None).elapsed_secs(now)
    }

    #[test_case(Some("Research") => Some((2, 2)); "declared_phase")]
    #[test_case(Some("Cleanup") => None; "undeclared_phase")]
    #[test_case(None => None; "no_phase")]
    fn phase_position_is_one_based_among_declared(phase: Option<&str>) -> Option<(usize, usize)> {
        snapshot(RunStatus::Active, phase).phase_position()
    }

    #[test]
    fn call_preview_cuts_on_a_character_boundary() {
        let text = "é".repeat(MAX_CALL_PREVIEW_BYTES);

        let preview = call_preview(&text);

        assert!(preview.ends_with(CALL_PREVIEW_MARKER));
        assert!(preview.len() <= MAX_CALL_PREVIEW_BYTES + CALL_PREVIEW_MARKER.len());
        assert_eq!(call_preview("short"), "short");
    }

    #[test]
    fn events_are_tagged_by_kind() {
        let event = WorkflowEvent::Log {
            run_id: RUN_ID.into(),
            revision: 3,
            at: 7,
            message: MESSAGE.into(),
        };

        let json = serde_json::to_value(&event).unwrap();

        assert_eq!(json["kind"], "log");
        assert_eq!(json["run_id"], RUN_ID);
        assert_eq!(json["message"], MESSAGE);
        let back: WorkflowEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }
}
