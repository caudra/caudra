use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::meta::MAX_SOURCE_BYTES;

pub const MAX_PAUSE_KIND_BYTES: usize = 32;

pub const DEFAULT_MAX_OPERATIONS: u64 = 50_000_000;
pub const DEFAULT_MAX_CALL_LEVELS: usize = 64;
pub const DEFAULT_MAX_EXPR_DEPTH: usize = 128;
pub const DEFAULT_MAX_STRING_SIZE: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_ARRAY_SIZE: usize = 65_536;
pub const DEFAULT_MAX_MAP_SIZE: usize = 65_536;
pub const DEFAULT_MAX_ARGS_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_MAX_HOST_CALLS: u64 = 10_000;
pub const DEFAULT_MAX_LOG_ENTRIES: u64 = 10_000;
pub const DEFAULT_MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_WALL_TIME: Duration = Duration::from_secs(4 * 60 * 60);

/// Why a workflow paused, as named by the script (for example `verification`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PauseKind(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PauseKindError {
    #[error("pause kind must not be empty")]
    Empty,
    #[error("pause kind must be at most {MAX_PAUSE_KIND_BYTES} bytes (got {0})")]
    TooLong(usize),
}

impl PauseKind {
    pub fn new(kind: impl Into<String>) -> Result<Self, PauseKindError> {
        let kind = kind.into();
        if kind.is_empty() {
            return Err(PauseKindError::Empty);
        }
        if kind.len() > MAX_PAUSE_KIND_BYTES {
            return Err(PauseKindError::TooLong(kind.len()));
        }
        Ok(Self(kind))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PauseKind {
    type Error = PauseKindError;

    fn try_from(kind: String) -> Result<Self, Self::Error> {
        Self::new(kind)
    }
}

impl From<PauseKind> for String {
    fn from(kind: PauseKind) -> Self {
        kind.0
    }
}

impl std::fmt::Display for PauseKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a workflow run ended. `Paused` runs resume by re-running the script against the journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub enum WorkflowOutcome {
    Completed(Value),
    Paused { kind: PauseKind, message: String },
    Failed(String),
    Cancelled,
    BudgetLimited,
}

/// Every bound the engine enforces on a single run.
///
/// `max_host_calls` counts result-bearing calls (`agent`, each `parallel` item, `write_scratch_file`);
/// `max_log_entries` and `max_log_bytes` cover `phase` and `log` emissions together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineLimits {
    pub max_operations: u64,
    pub max_call_levels: usize,
    pub max_expr_depth: usize,
    pub max_string_size: usize,
    pub max_array_size: usize,
    pub max_map_size: usize,
    pub max_source_bytes: usize,
    pub max_args_bytes: usize,
    pub max_output_bytes: usize,
    pub max_host_calls: u64,
    pub max_log_entries: u64,
    pub max_log_bytes: usize,
    pub wall_time: Duration,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_operations: DEFAULT_MAX_OPERATIONS,
            max_call_levels: DEFAULT_MAX_CALL_LEVELS,
            max_expr_depth: DEFAULT_MAX_EXPR_DEPTH,
            max_string_size: DEFAULT_MAX_STRING_SIZE,
            max_array_size: DEFAULT_MAX_ARRAY_SIZE,
            max_map_size: DEFAULT_MAX_MAP_SIZE,
            max_source_bytes: MAX_SOURCE_BYTES,
            max_args_bytes: DEFAULT_MAX_ARGS_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_host_calls: DEFAULT_MAX_HOST_CALLS,
            max_log_entries: DEFAULT_MAX_LOG_ENTRIES,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            wall_time: DEFAULT_WALL_TIME,
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("" => Err(PauseKindError::Empty); "empty")]
    #[test_case("verification" => Ok(()); "plain")]
    #[test_case("a" => Ok(()); "single_byte")]
    fn pause_kind_validation(kind: &str) -> Result<(), PauseKindError> {
        PauseKind::new(kind).map(|_| ())
    }

    #[test]
    fn pause_kind_rejects_oversized() {
        let kind = "k".repeat(MAX_PAUSE_KIND_BYTES + 1);
        assert_eq!(
            PauseKind::new(kind),
            Err(PauseKindError::TooLong(MAX_PAUSE_KIND_BYTES + 1))
        );
    }

    #[test]
    fn pause_kind_serde_round_trip_validates() {
        let kind: PauseKind = serde_json::from_str("\"user\"").expect("valid kind");
        assert_eq!(kind.as_str(), "user");
        assert!(serde_json::from_str::<PauseKind>("\"\"").is_err());
    }

    #[test]
    fn outcome_serializes_tagged() {
        let outcome = WorkflowOutcome::Paused {
            kind: PauseKind::new("user").expect("valid kind"),
            message: "waiting".into(),
        };
        let json = serde_json::to_value(&outcome).expect("serializable");
        assert_eq!(json["outcome"], "paused");
        assert_eq!(json["detail"]["kind"], "user");
        let back: WorkflowOutcome = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, outcome);
    }
}
