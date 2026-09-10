//! Runtime-neutral views of a workflow run: what the UI, the SDK, and the
//! journal agree a run looks like, independent of the script engine.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const WORKFLOW_LANGUAGE_VERSION: u32 = 1;
pub const WORKFLOW_ABI_VERSION: u32 = 1;
pub const DEFAULT_AGENT_BUDGET: u32 = 128;
pub const MAX_AGENT_BUDGET: u32 = 256;
pub const MAX_ACTIVE_RUNS: usize = 4;
pub const MAX_RUN_LOG_ENTRIES: usize = 200;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run_id: String,
    pub display_name: String,
    pub workflow_name: String,
    pub source_kind: SourceKind,
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
    pub logs: Vec<String>,
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
        message: String,
    },
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

    #[test]
    fn events_are_tagged_by_kind() {
        let event = WorkflowEvent::Log {
            run_id: RUN_ID.into(),
            revision: 3,
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
