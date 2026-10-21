//! The watcher's vocabulary: GitHub facts, per-PR snapshots, decisions, and the verdicts it prints,
//! serialized in the same key order upstream's `JSON.stringify` writes them.

use std::sync::LazyLock;

use regex::Regex;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use crate::host::Cancelled;

pub const SCHEMA_VERSION: u8 = 1;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const BYTE_ORDER_MARK: char = '\u{feff}';
const INFINITY: &str = "Infinity";

static DECIMAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[+-]?([0-9]+\.?[0-9]*|\.[0-9]+)([eE][+-]?[0-9]+)?$").expect("literal")
});

pub type PrNumber = u64;

/// A positive integer, as upstream's `parsePrNumber` accepts it from a JavaScript number.
pub fn pr_number(value: f64) -> Option<PrNumber> {
    (value.is_finite() && value.fract() == 0.0 && value > 0.0).then_some(value as PrNumber)
}

/// JavaScript's `Number(text)`: decimal, `0x`/`0o`/`0b`, or `Infinity`, padded by whitespace, with
/// blank text as zero and anything else as `NaN`.
pub fn string_to_number(text: &str) -> f64 {
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == BYTE_ORDER_MARK);
    if text.is_empty() {
        return 0.0;
    }
    let radix = [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ]
    .into_iter()
    .find_map(|(prefix, radix)| text.strip_prefix(prefix).map(|digits| (digits, radix)));
    if let Some((digits, radix)) = radix {
        return digits
            .chars()
            .try_fold(0.0, |total: f64, digit| {
                digit
                    .to_digit(radix)
                    .map(|digit| total * f64::from(radix) + f64::from(digit))
            })
            .filter(|_| !digits.is_empty())
            .unwrap_or(f64::NAN);
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == INFINITY {
        return if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if DECIMAL.is_match(text) {
        text.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `value` as `JSON.stringify` writes a number: integral values carry no fraction.
pub fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
        return Value::from(value as i64);
    }
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repository {
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrContext {
    pub owner: String,
    pub repo: String,
    pub number: PrNumber,
}

impl PrContext {
    pub fn with_number(&self, number: PrNumber) -> Self {
        Self {
            number,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeStateStatus {
    Behind,
    Blocked,
    Clean,
    Conflicting,
    Dirty,
    Draft,
    HasHooks,
    Unknown,
    Unstable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RollupState {
    Error,
    Expected,
    Failure,
    Pending,
    Success,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestFacts {
    pub context: PrContext,
    pub mergeable: MergeableState,
    pub merge_state_status: MergeStateStatus,
    pub review_decision: Option<ReviewDecision>,
    pub head_ref_oid: Option<String>,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub state: PrState,
    pub merged_at: Option<String>,
    pub is_draft: bool,
}

impl PullRequestFacts {
    pub fn has_conflicts(&self) -> bool {
        self.mergeable == MergeableState::Conflicting
            || matches!(
                self.merge_state_status,
                MergeStateStatus::Dirty | MergeStateStatus::Conflicting
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPullRequest {
    pub number: PrNumber,
    pub head_ref_name: String,
    pub base_ref_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    pub author_login: Option<String>,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub id: String,
    pub first_comment: Option<ReviewComment>,
    pub is_bugbot: bool,
    pub bugbot_review_passes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawReviewThread {
    pub id: String,
    pub first_comment: Option<ReviewComment>,
    pub resolved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    Passed,
    Skipped,
    Failed,
    Pending,
    CodeReviewGate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    pub description: String,
    pub link: String,
    pub workflow: String,
    pub kind: CheckKind,
    pub reported_state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckSource {
    GhPrChecks,
    GraphqlRollup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRead {
    pub source: CheckSource,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRollup {
    pub oid: String,
    pub state: Option<RollupState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergeBasis {
    MergeState,
    Rollup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum GitHubMergeAssessment {
    Allowed {
        basis: MergeBasis,
        merge_state_status: MergeStateStatus,
        head_rollup_state: Option<RollupState>,
    },
    Refused {
        merge_state_status: MergeStateStatus,
        head_rollup_state: Option<RollupState>,
    },
}

impl GitHubMergeAssessment {
    pub fn merge_state_status(self) -> MergeStateStatus {
        match self {
            Self::Allowed {
                merge_state_status, ..
            }
            | Self::Refused {
                merge_state_status, ..
            } => merge_state_status,
        }
    }

    pub fn head_rollup_state(self) -> Option<RollupState> {
        match self {
            Self::Allowed {
                head_rollup_state, ..
            }
            | Self::Refused {
                head_rollup_state, ..
            } => head_rollup_state,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CiKind {
    #[serde(rename = "ci-failing")]
    Failing,
    #[serde(rename = "ci-github-rejected")]
    GithubRejected,
    #[serde(rename = "ci-pending")]
    Pending,
    #[serde(rename = "ci-clean")]
    Clean,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiState {
    pub source: CheckSource,
    pub all: Vec<Check>,
    pub had_previous_passing_ci: bool,
    pub kind: CiKind,
    pub failed: Vec<Check>,
    pub pending: Vec<Check>,
    pub github: Option<GitHubMergeAssessment>,
    /// Pending checks were read without commit history, which upstream also writes in its own
    /// key order.
    pub history_omitted: bool,
}

impl Serialize for CiState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if self.history_omitted {
            map.serialize_entry("kind", &self.kind)?;
            map.serialize_entry("source", &self.source)?;
            map.serialize_entry("all", &self.all)?;
        } else {
            map.serialize_entry("source", &self.source)?;
            map.serialize_entry("all", &self.all)?;
            map.serialize_entry("hadPreviousPassingCi", &self.had_previous_passing_ci)?;
            map.serialize_entry("kind", &self.kind)?;
        }
        map.serialize_entry("failed", &self.failed)?;
        map.serialize_entry("pending", &self.pending)?;
        if self.history_omitted {
            map.serialize_entry("hadPreviousPassingCi", &self.had_previous_passing_ci)?;
        }
        if let Some(github) = &self.github {
            map.serialize_entry("github", github)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum PrSnapshot {
    Merged {
        context: PrContext,
        facts: PullRequestFacts,
    },
    Closed {
        context: PrContext,
        facts: PullRequestFacts,
    },
    Open {
        context: PrContext,
        facts: PullRequestFacts,
        threads: Vec<ReviewThread>,
        ci: CiState,
        review_automation_running: bool,
    },
}

impl PrSnapshot {
    pub fn context(&self) -> &PrContext {
        match self {
            Self::Merged { context, .. }
            | Self::Closed { context, .. }
            | Self::Open { context, .. } => context,
        }
    }

    pub fn is_merged(&self) -> bool {
        matches!(self, Self::Merged { .. })
    }

    /// The pending checks of an open row whose CI is still running.
    pub fn pending_ci(&self) -> Option<&[Check]> {
        match self {
            Self::Open { ci, .. } if ci.kind == CiKind::Pending => Some(&ci.pending),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mergeability {
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DraftGate {
    NotDraft,
    DraftAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyGate {
    pub state: PrState,
    pub review_decision: Option<ReviewDecision>,
    pub draft: DraftGate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadyProof {
    pub mergeability: Mergeability,
    pub threads: [ReviewThread; 0],
    pub ci: CiState,
    pub gate: ReadyGate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename = "ready-pr")]
pub struct ReadyPr {
    pub context: PrContext,
    pub proof: ReadyProof,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename = "merged-pr", rename_all = "camelCase")]
pub struct MergedPr {
    pub context: PrContext,
    pub merged_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Settled {
    Ready(ReadyPr),
    Merged(MergedPr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergeGateReason {
    ClosedWithoutMerge,
    DraftPr,
    ChangesRequested,
}

/// Why a PR cannot merge, or why its status could not be read at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Blocker {
    MergeConflicts {
        pr: PrContext,
        facts: PullRequestFacts,
    },
    ReviewThreads {
        pr: PrContext,
        threads: Vec<ReviewThread>,
    },
    FailingChecks {
        pr: PrContext,
        ci: CiState,
    },
    MergeGate {
        pr: PrContext,
        reason: MergeGateReason,
    },
    StatusQuery {
        failures: u64,
        failure: QueryFailure,
    },
}

impl Blocker {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::MergeConflicts { .. } => 2,
            Self::ReviewThreads { .. } => 3,
            Self::FailingChecks { .. } => 4,
            Self::MergeGate { .. } => 6,
            Self::StatusQuery { .. } => 7,
        }
    }
}

/// A GitHub read that failed in a way the watcher reports instead of crashing on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryFailure {
    JsonParse {
        detail: String,
    },
    MissingKey {
        detail: String,
        raw_value: Option<String>,
    },
    CommandExit {
        code: i32,
        detail: String,
    },
    ChecksUnavailable {
        detail: String,
    },
    InvalidContextUrl {
        raw_value: String,
        detail: String,
    },
}

impl QueryFailure {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::JsonParse { .. } => "json-parse",
            Self::MissingKey { .. } => "missing-key",
            Self::CommandExit { .. } => "command-exit",
            Self::ChecksUnavailable { .. } => "checks-unavailable",
            Self::InvalidContextUrl { .. } => "invalid-context-url",
        }
    }

    pub fn retryable(&self) -> bool {
        !matches!(self, Self::InvalidContextUrl { .. })
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::JsonParse { detail }
            | Self::MissingKey { detail, .. }
            | Self::CommandExit { detail, .. }
            | Self::ChecksUnavailable { detail }
            | Self::InvalidContextUrl { detail, .. } => detail,
        }
    }
}

impl Serialize for QueryFailure {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("kind", self.kind())?;
        map.serialize_entry("retryable", &self.retryable())?;
        match self {
            Self::JsonParse { detail } | Self::ChecksUnavailable { detail } => {
                map.serialize_entry("detail", detail)?;
            }
            Self::MissingKey { detail, raw_value } => {
                map.serialize_entry("detail", detail)?;
                if let Some(raw_value) = raw_value {
                    map.serialize_entry("rawValue", raw_value)?;
                }
            }
            Self::CommandExit { code, detail } => {
                map.serialize_entry("code", code)?;
                map.serialize_entry("detail", detail)?;
            }
            Self::InvalidContextUrl { raw_value, detail } => {
                map.serialize_entry("rawValue", raw_value)?;
                map.serialize_entry("detail", detail)?;
            }
        }
        map.end()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("{}", .0.detail())]
    Query(QueryFailure),
    /// What upstream lets escape as an uncaught exception: a command that cannot start, or a
    /// broken invariant.
    #[error("{0}")]
    Fatal(String),
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WatchMode {
    Single,
    Stack,
    QueuedStack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusReason {
    Poll,
    WholeStackSweep,
    StatusOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WaitReason {
    PendingChecks { pending: Vec<Check> },
    MergeQueue { unmerged_count: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum TimeoutReason {
    PendingChecks {
        pending: Vec<Check>,
    },
    StatusUnavailable {
        failure: QueryFailure,
    },
    QueuedStack {
        frontier: PrContext,
        unmerged_count: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ReadyScope {
    Single { pr: Settled },
    Stack { prs: Vec<Settled> },
}

/// A verdict's payload; [`Event::exit_code`] tells the terminal ones from progress.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Queue {
        queue: Vec<PrContext>,
    },
    Status {
        reason: StatusReason,
        rows: Vec<PrSnapshot>,
    },
    Waiting {
        frontier: PrContext,
        reason: WaitReason,
    },
    Advance {
        merged: PrContext,
        frontier: PrContext,
        remaining: usize,
    },
    Retry {
        failure: QueryFailure,
        consecutive_failures: u64,
        retry_in_seconds: f64,
    },
    Blocker(Blocker),
    Ready(ReadyScope),
    Complete {
        queue: Vec<PrContext>,
        merged: Vec<MergedPr>,
    },
    Timeout(TimeoutReason),
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Queue { .. } => "QUEUE",
            Self::Status { .. } => "STATUS",
            Self::Waiting { .. } => "WAITING",
            Self::Advance { .. } => "ADVANCE",
            Self::Retry { .. } => "RETRY",
            Self::Blocker(_) => "BLOCKER",
            Self::Ready(_) => "READY",
            Self::Complete { .. } => "COMPLETE",
            Self::Timeout(_) => "TIMEOUT",
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Status {
                reason: StatusReason::StatusOnly,
                ..
            }
            | Self::Ready(_)
            | Self::Complete { .. } => Some(0),
            Self::Blocker(blocker) => Some(blocker.exit_code()),
            Self::Timeout(_) => Some(5),
            Self::Queue { .. }
            | Self::Status { .. }
            | Self::Waiting { .. }
            | Self::Advance { .. }
            | Self::Retry { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub sequence: u64,
    pub observed_at: String,
    pub mode: WatchMode,
    pub event: Event,
}

impl Serialize for Verdict {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("schemaVersion", &SCHEMA_VERSION)?;
        map.serialize_entry("sequence", &self.sequence)?;
        map.serialize_entry("observedAt", &self.observed_at)?;
        map.serialize_entry("mode", &self.mode)?;
        map.serialize_entry("kind", self.event.kind())?;
        let exit_code = self.event.exit_code();
        map.serialize_entry("terminal", &exit_code.is_some())?;
        if let Some(exit_code) = exit_code {
            map.serialize_entry("exitCode", &exit_code)?;
        }
        match &self.event {
            Event::Queue { queue } => map.serialize_entry("queue", queue)?,
            Event::Status { reason, rows } => {
                map.serialize_entry("reason", reason)?;
                map.serialize_entry("rows", rows)?;
            }
            Event::Waiting { frontier, reason } => {
                map.serialize_entry("frontier", frontier)?;
                map.serialize_entry("reason", reason)?;
            }
            Event::Advance {
                merged,
                frontier,
                remaining,
            } => {
                map.serialize_entry("merged", merged)?;
                map.serialize_entry("frontier", frontier)?;
                map.serialize_entry("remaining", remaining)?;
            }
            Event::Retry {
                failure,
                consecutive_failures,
                retry_in_seconds,
            } => {
                map.serialize_entry("failure", failure)?;
                map.serialize_entry("consecutiveFailures", consecutive_failures)?;
                map.serialize_entry("retryInSeconds", &js_number(*retry_in_seconds))?;
            }
            Event::Blocker(blocker) => map.serialize_entry("blocker", blocker)?,
            Event::Ready(scope) => map.serialize_entry("scope", scope)?,
            Event::Complete { queue, merged } => {
                map.serialize_entry("queue", queue)?;
                map.serialize_entry("merged", merged)?;
            }
            Event::Timeout(reason) => map.serialize_entry("reason", reason)?,
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksFastPath {
    Checks(Vec<Check>),
    Unusable { exit_code: i32, stderr: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RollupPage {
    pub checks: Vec<Check>,
    pub end_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewThreadPage {
    pub threads: Vec<RawReviewThread>,
    pub end_cursor: Option<String>,
}

/// Everything the watcher reads from GitHub.
pub trait GitHubReader {
    fn origin_repo(&self) -> Result<Option<Repository>, WatchError>;
    fn current_pr(&self, pr: Option<PrNumber>) -> Result<PrContext, WatchError>;
    fn pull_request(&self, context: &PrContext) -> Result<PullRequestFacts, WatchError>;
    fn open_pull_requests(&self, context: &PrContext) -> Result<Vec<OpenPullRequest>, WatchError>;
    fn checks_fast_path(&self, context: &PrContext) -> Result<ChecksFastPath, WatchError>;
    fn check_rollup_page(
        &self,
        context: &PrContext,
        after: Option<&str>,
    ) -> Result<RollupPage, WatchError>;
    fn review_thread_page(
        &self,
        context: &PrContext,
        after: Option<&str>,
    ) -> Result<ReviewThreadPage, WatchError>;
    fn commit_rollups(&self, context: &PrContext) -> Result<Vec<CommitRollup>, WatchError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct PollingOptions {
    pub interval: f64,
    pub sweep_interval: f64,
    pub timeout: f64,
    pub max_query_errors: u64,
    pub allow_draft: bool,
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("42", 42.0 ; "integer")]
    #[test_case(" 2.5\n", 2.5 ; "padded_decimal")]
    #[test_case("", 0.0 ; "blank_is_zero")]
    #[test_case("0x10", 16.0 ; "hex")]
    #[test_case("0b101", 5.0 ; "binary")]
    #[test_case("1e3", 1000.0 ; "exponent")]
    #[test_case(".5", 0.5 ; "leading_dot")]
    #[test_case("-Infinity", f64::NEG_INFINITY ; "negative_infinity")]
    fn reads_numbers_like_javascript(text: &str, expected: f64) {
        assert_eq!(string_to_number(text), expected);
    }

    #[test_case("abc" ; "word")]
    #[test_case("1_000" ; "separator")]
    #[test_case("inf" ; "rust_infinity")]
    #[test_case("0x" ; "bare_prefix")]
    #[test_case("-0x10" ; "signed_hex")]
    fn rejects_what_javascript_reads_as_nan(text: &str) {
        assert!(string_to_number(text).is_nan());
    }

    #[test_case(60.0, "60" ; "integral")]
    #[test_case(90.5, "90.5" ; "fractional")]
    fn writes_numbers_like_json_stringify(value: f64, expected: &str) {
        assert_eq!(js_number(value).to_string(), expected);
    }
}
