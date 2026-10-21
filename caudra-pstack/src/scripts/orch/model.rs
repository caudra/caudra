//! The rows orch keeps, shaped as upstream's `--json` prints them, and the errors that decide its
//! exit code.

use std::io;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

use super::compat::{locale_compare, object_key_order};

pub const VERDICTS: [&str; 5] = [
    "live-ui-verified",
    "unit-test-verified",
    "type-check-only",
    "verifier-blocked",
    "verifier-failed",
];
const VERDICT_ERROR: &str = "verdict must be live-ui-verified, unit-test-verified, type-check-only, verifier-blocked, or verifier-failed";

#[derive(Debug, thiserror::Error)]
pub enum OrchError {
    #[error("{0}")]
    User(String),
    /// A user error that also prints the program's help.
    #[error("{0}")]
    Usage(String),
    /// Exit 2: printed as `output` on stdout when present, else as an error.
    #[error("{message}")]
    NotFound {
        message: String,
        output: Option<NotFoundOutput>,
    },
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl OrchError {
    pub fn user(message: impl Into<String>) -> Self {
        Self::User(message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
            output: None,
        }
    }
}

#[derive(Debug)]
pub struct NotFoundOutput {
    pub compact: String,
    pub json: Value,
}

pub type OrchResult<T> = Result<T, OrchError>;

pub fn parse_verdict(value: &str) -> OrchResult<&'static str> {
    VERDICTS
        .into_iter()
        .find(|verdict| *verdict == value)
        .ok_or_else(|| OrchError::user(VERDICT_ERROR))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unit {
    pub id: String,
    pub track: String,
    pub state: String,
    pub branch: String,
    pub pr: String,
    pub sha: String,
    pub brief: String,
}

impl Unit {
    pub fn cells(&self) -> [&str; 7] {
        [
            &self.id,
            &self.track,
            &self.state,
            &self.branch,
            &self.pr,
            &self.sha,
            &self.brief,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerEntry {
    pub pr: String,
    pub sha: String,
    pub verdict: &'static str,
    pub evidence: String,
    pub verifier: String,
    pub ts: String,
}

impl LedgerEntry {
    pub fn cells(&self) -> [&str; 6] {
        [
            &self.pr,
            &self.sha,
            self.verdict,
            &self.evidence,
            &self.verifier,
            &self.ts,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InboxPointer {
    pub ts: String,
    pub agent: String,
    pub unit: String,
    pub status: String,
    pub report: String,
}

impl InboxPointer {
    pub fn cells(&self) -> [&str; 5] {
        [
            &self.ts,
            &self.agent,
            &self.unit,
            &self.status,
            &self.report,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxPushResult {
    pub pointer: InboxPointer,
    pub filename: String,
}

/// A parked decision; `answer` is set once it is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub id: String,
    pub question: String,
    pub options: String,
    pub default_answer: String,
    pub answer: Option<String>,
}

impl Gate {
    pub fn kind(&self) -> &'static str {
        if self.answer.is_some() {
            "resolved"
        } else {
            "open"
        }
    }
}

impl Serialize for Gate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("kind", self.kind())?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("question", &self.question)?;
        map.serialize_entry("options", &self.options)?;
        map.serialize_entry("defaultAnswer", &self.default_answer)?;
        if let Some(answer) = &self.answer {
            map.serialize_entry("answer", answer)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

impl PrState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Open, Self::Merged, Self::Closed]
            .into_iter()
            .find(|state| state.as_str() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrontierPr {
    pub pr: u64,
    pub branches: String,
    pub sha: String,
    pub state: PrState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Frontier {
    pub generation: u64,
    pub prs: Vec<FrontierPr>,
    pub lowest_unmerged: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StandingLine {
    pub number: usize,
    pub line: String,
}

/// How often each value occurs, ordered as upstream's sorted `Object.fromEntries` iterates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts(pub Vec<(String, u64)>);

impl Counts {
    pub fn of<'a>(values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut entries: Vec<(String, u64)> = Vec::new();
        for value in values {
            match entries.iter_mut().find(|(name, _)| name == value) {
                Some((_, count)) => *count += 1,
                None => entries.push((value.to_owned(), 1)),
            }
        }
        entries.sort_by(|(left, _), (right, _)| locale_compare(left, right));
        object_key_order(&mut entries);
        Self(entries)
    }

    pub fn get(&self, name: &str) -> u64 {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map_or(0, |(_, count)| *count)
    }

    /// `name=count, ...`, or `none`.
    pub fn line(&self) -> String {
        if self.0.is_empty() {
            return "none".to_owned();
        }
        self.0
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Serialize for Counts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(name, count)| (name, count)))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSummary {
    pub unit_states: Counts,
    pub ledger_verdicts: Counts,
    #[serde(serialize_with = "js_number")]
    pub frontier_generation: f64,
    pub open_gate_ids: Vec<String>,
}

/// A number as `JSON.stringify` writes it: integral values carry no fraction.
fn js_number<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub units: Vec<Unit>,
    pub ledger: Vec<LedgerEntry>,
    pub frontier: Frontier,
    pub gates: Vec<Gate>,
    pub summary: StatusSummary,
    pub changed: String,
}
