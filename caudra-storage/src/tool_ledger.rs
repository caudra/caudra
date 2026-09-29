//! What tools did, kept apart from the sessions that produced them.
//!
//! The same split spend uses: `session_tool_usage` dies with its transcript
//! through a foreign key, which is right for a transcript and wrong for a
//! project's history, so every call also lands here in an hourly bucket that
//! names no session. Like the usage ledger, this always writes to the
//! persistent root, so an ephemeral run's activity is not discarded with its
//! volatile directory.
//!
//! Unlike spend, an ephemeral call is not marked. Nothing about it needs
//! telling apart: a call that ran against a project ran against that project
//! whether or not its transcript was kept.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::sessions::{
    SessionDatabase, SessionError, StoredToolUsage, ToolBucket, ToolLedgerEntry,
};
use crate::usage_ledger::bucket_for;
use crate::{StateClass, StateDir, now_epoch};

/// Slowest-first, and bounded so a modal never renders an unbounded table.
const FAILURE_LIMIT: usize = 4;
/// Eight sub-buckets per octave, which holds a percentile within about 12% of
/// the duration it names and keeps an encoded histogram to a few bytes.
const LATENCY_SUB_BITS: u32 = 3;
const LATENCY_SUB: u64 = 1 << LATENCY_SUB_BITS;
const LATENCY_CORRUPT: &str = "malformed latency histogram";
const P50: f64 = 0.5;
const P95: f64 = 0.95;
const VARINT_CONTINUE: u8 = 0x80;
const VARINT_PAYLOAD_BITS: u32 = 7;

/// How a call ended. `Ok` plus the low-cardinality classes a failing tool
/// states for itself, so the storage grain stays bounded at seven rows per key
/// and an error rate can be read as a reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok,
    Cancelled,
    Timeout,
    Denied,
    NotFound,
    InvalidInput,
    Other,
}

impl ToolOutcome {
    pub const fn storage_name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Denied => "denied",
            Self::NotFound => "not_found",
            Self::InvalidInput => "invalid_input",
            Self::Other => "other",
        }
    }

    pub fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "ok" => Some(Self::Ok),
            "cancelled" => Some(Self::Cancelled),
            "timeout" => Some(Self::Timeout),
            "denied" => Some(Self::Denied),
            "not_found" => Some(Self::NotFound),
            "invalid_input" => Some(Self::InvalidInput),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    pub const fn is_error(self) -> bool {
        !matches!(self, Self::Ok)
    }
}

/// Durations as a log-scale histogram, because a percentile has to survive
/// being merged across hourly buckets the way a sum does. Counts rather than
/// samples, so an hour costs the same few bytes whether it saw ten calls or
/// ten thousand.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latency {
    counts: BTreeMap<u16, u64>,
}

impl Latency {
    pub fn of(duration_ms: u64) -> Self {
        let mut latency = Self::default();
        latency.record(duration_ms);
        latency
    }

    pub fn record(&mut self, duration_ms: u64) {
        self.add(bucket_of(duration_ms), 1);
    }

    pub fn merge(&mut self, other: &Self) {
        for (bucket, count) in &other.counts {
            self.add(*bucket, *count);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// The duration at or below which `fraction` of the calls finished, given
    /// as the top of its bucket so the answer is never optimistic. `None` when
    /// nothing was recorded, which differs from a tool that was always instant.
    pub fn percentile(&self, fraction: f64) -> Option<u64> {
        let total: u64 = self.counts.values().copied().sum();
        if total == 0 {
            return None;
        }
        let rank = ((total as f64 * fraction).ceil() as u64).max(1);
        let mut seen: u64 = 0;
        for (bucket, count) in &self.counts {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return Some(bucket_ceiling(*bucket));
            }
        }
        self.counts.keys().next_back().copied().map(bucket_ceiling)
    }

    /// Ascending pairs of bucket gap and count, each one a LEB128 varint. Calls
    /// that cluster, which is most of them, cost a handful of bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut previous: u64 = 0;
        for (bucket, count) in &self.counts {
            let bucket = u64::from(*bucket);
            put_varint(&mut bytes, bucket - previous);
            put_varint(&mut bytes, *count);
            previous = bucket;
        }
        bytes
    }

    pub fn decode(bytes: &[u8], field: &'static str) -> Result<Self, SessionError> {
        let mut counts = BTreeMap::new();
        let mut cursor = 0;
        let mut bucket: u64 = 0;
        while cursor < bytes.len() {
            let (Some(gap), Some(count)) = (
                take_varint(bytes, &mut cursor),
                take_varint(bytes, &mut cursor),
            ) else {
                return Err(corrupt_latency(field));
            };
            bucket = bucket.saturating_add(gap);
            counts.insert(
                u16::try_from(bucket).map_err(|_| corrupt_latency(field))?,
                count,
            );
        }
        Ok(Self { counts })
    }

    fn add(&mut self, bucket: u16, count: u64) {
        let total = self.counts.entry(bucket).or_default();
        *total = total.saturating_add(count);
    }
}

/// Which histogram bucket a duration falls in. Below `LATENCY_SUB` every
/// millisecond is its own bucket, and above it each octave is cut into
/// `LATENCY_SUB` even parts.
fn bucket_of(duration_ms: u64) -> u16 {
    if duration_ms < LATENCY_SUB {
        return duration_ms as u16;
    }
    let shift = u64::BITS - 1 - duration_ms.leading_zeros() - LATENCY_SUB_BITS;
    let sub = (duration_ms >> shift) - LATENCY_SUB;
    ((u64::from(shift) + 1) * LATENCY_SUB + sub) as u16
}

fn bucket_floor(bucket: u16) -> u64 {
    let bucket = u64::from(bucket);
    if bucket < LATENCY_SUB {
        return bucket;
    }
    (LATENCY_SUB + bucket % LATENCY_SUB)
        .checked_shl((bucket / LATENCY_SUB - 1) as u32)
        .unwrap_or(u64::MAX)
}

/// The largest duration the bucket can hold, which is what a percentile
/// reports.
fn bucket_ceiling(bucket: u16) -> u64 {
    bucket_floor(bucket.saturating_add(1)).saturating_sub(1)
}

fn put_varint(bytes: &mut Vec<u8>, mut value: u64) {
    while value >= u64::from(VARINT_CONTINUE) {
        bytes.push(value as u8 | VARINT_CONTINUE);
        value >>= VARINT_PAYLOAD_BITS;
    }
    bytes.push(value as u8);
}

fn take_varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & !VARINT_CONTINUE) << shift;
        if byte & VARINT_CONTINUE == 0 {
            return Some(value);
        }
        shift += VARINT_PAYLOAD_BITS;
        if shift >= u64::BITS {
            return None;
        }
    }
}

fn corrupt_latency(field: &'static str) -> SessionError {
    SessionError::CorruptDatabaseValue {
        field,
        reason: LATENCY_CORRUPT.to_owned(),
    }
}

/// One finished call, before it is folded into its bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub tool: String,
    /// Where the tool came from, as the agent reports it: `native`, a Lua
    /// plugin, or an MCP server. Two tools can share a name across sources.
    pub source: String,
    pub cwd: String,
    pub outcome: ToolOutcome,
    pub duration_ms: u64,
    /// Estimated size of the model-facing result, which is what the call cost
    /// the context window.
    pub tokens: u32,
}

/// A handle for several ledger operations in a row, held open across a run
/// rather than paying for a connection per call.
pub struct ToolLedger {
    database: SessionDatabase,
}

impl ToolLedger {
    pub fn open(dir: &StateDir) -> Result<Self, SessionError> {
        Ok(Self {
            database: SessionDatabase::open_state(&dir.for_class(StateClass::Persistent))?,
        })
    }

    pub fn record(&self, call: &ToolCall) -> Result<(), SessionError> {
        self.record_at(call, now_epoch())
    }

    pub fn record_at(&self, call: &ToolCall, now: u64) -> Result<(), SessionError> {
        self.database.record_tool_call(&ToolLedgerEntry {
            bucket_start: bucket_for(now),
            tool: &call.tool,
            source: &call.source,
            cwd: &call.cwd,
            outcome: call.outcome,
            calls: 1,
            duration_ms: call.duration_ms,
            tokens: u64::from(call.tokens),
            latency: &Latency::of(call.duration_ms),
        })
    }

    pub fn buckets(
        &self,
        since: Option<i64>,
        cwd: Option<&str>,
    ) -> Result<Vec<ToolBucket>, SessionError> {
        self.database.tool_buckets(since, cwd)
    }

    pub fn prune_before(&self, bucket_start: i64) -> Result<usize, SessionError> {
        self.database.prune_tool_calls_before(bucket_start)
    }

    /// Everything ever recorded, in every project.
    pub fn lifetime(&self) -> Result<ToolStats, SessionError> {
        Ok(summarize(&self.buckets(None, None)?))
    }

    /// Everything recorded against one exact working directory.
    pub fn project(&self, cwd: &str) -> Result<ToolStats, SessionError> {
        Ok(summarize(&self.buckets(None, Some(cwd))?))
    }
}

/// What one tool did over whatever scope produced it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ToolSlice {
    pub tool: String,
    pub calls: u64,
    pub errors: u64,
    pub duration_ms: u64,
    pub tokens: u64,
    pub latency: Latency,
    /// The failure classes behind `errors`, largest first, so a rate reads as
    /// a reason. Capped: a modal row cannot grow without bound.
    pub failures: Vec<(ToolOutcome, u64)>,
}

impl ToolSlice {
    /// `None` when the tool was never called, which is different from a tool
    /// that never failed.
    pub fn error_rate(&self) -> Option<f64> {
        (self.calls > 0).then(|| self.errors as f64 / self.calls as f64)
    }

    pub fn mean_duration_ms(&self) -> Option<u64> {
        (self.calls > 0).then(|| self.duration_ms / self.calls)
    }

    /// The typical call. Worth reading beside `mean_duration_ms`, because the
    /// gap between them is what a few slow calls did to the average.
    pub fn p50_duration_ms(&self) -> Option<u64> {
        self.latency.percentile(P50)
    }

    /// The tail the mean hides. Read as an upper bound: the histogram knows
    /// which bucket the call landed in rather than its exact duration.
    pub fn p95_duration_ms(&self) -> Option<u64> {
        self.latency.percentile(P95)
    }
}

/// Tool activity over one scope, and the totals its shares are taken against.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ToolStats {
    pub calls: u64,
    pub errors: u64,
    pub duration_ms: u64,
    pub tokens: u64,
    /// Ranked by call count, which is the column the table is sorted on.
    pub by_tool: Vec<ToolSlice>,
}

impl ToolStats {
    pub fn is_empty(&self) -> bool {
        self.calls == 0
    }

    /// The session-scoped answer, from rows the session carries itself. The
    /// ledger cannot give this one: it names no session on purpose.
    pub fn from_session(rows: &[StoredToolUsage]) -> Self {
        fold(rows.iter().map(|row| Contribution {
            tool: &row.tool,
            outcome: row.outcome,
            calls: row.calls,
            duration_ms: row.duration_ms,
            tokens: row.tokens,
            latency: &row.latency,
        }))
    }
}

/// What one stored row contributes, in the shape both sources share.
struct Contribution<'a> {
    tool: &'a str,
    outcome: ToolOutcome,
    calls: u64,
    duration_ms: u64,
    tokens: u64,
    latency: &'a Latency,
}

fn summarize(buckets: &[ToolBucket]) -> ToolStats {
    fold(buckets.iter().map(|bucket| Contribution {
        tool: &bucket.tool,
        outcome: bucket.outcome,
        calls: bucket.calls,
        duration_ms: bucket.duration_ms,
        tokens: bucket.tokens,
        latency: &bucket.latency,
    }))
}

fn fold<'a>(contributions: impl Iterator<Item = Contribution<'a>>) -> ToolStats {
    let mut slices: BTreeMap<String, ToolSlice> = BTreeMap::new();
    for contribution in contributions {
        add(
            slices.entry(contribution.tool.to_owned()).or_default(),
            &contribution,
        );
    }
    finish(slices)
}

fn add(slice: &mut ToolSlice, contribution: &Contribution) {
    if slice.tool.is_empty() {
        slice.tool = contribution.tool.to_owned();
    }
    slice.calls = slice.calls.saturating_add(contribution.calls);
    slice.duration_ms = slice.duration_ms.saturating_add(contribution.duration_ms);
    slice.tokens = slice.tokens.saturating_add(contribution.tokens);
    slice.latency.merge(contribution.latency);
    if !contribution.outcome.is_error() {
        return;
    }
    slice.errors = slice.errors.saturating_add(contribution.calls);
    match slice
        .failures
        .iter_mut()
        .find(|(class, _)| *class == contribution.outcome)
    {
        Some((_, count)) => *count = count.saturating_add(contribution.calls),
        None => slice
            .failures
            .push((contribution.outcome, contribution.calls)),
    }
}

fn finish(slices: BTreeMap<String, ToolSlice>) -> ToolStats {
    let mut stats = ToolStats::default();
    let mut by_tool: Vec<ToolSlice> = slices.into_values().collect();
    for slice in &mut by_tool {
        stats.calls = stats.calls.saturating_add(slice.calls);
        stats.errors = stats.errors.saturating_add(slice.errors);
        stats.duration_ms = stats.duration_ms.saturating_add(slice.duration_ms);
        stats.tokens = stats.tokens.saturating_add(slice.tokens);
        // Ties break on the class name so the same data always renders the
        // same way; a BTreeMap gave the tools that order already.
        slice
            .failures
            .sort_by(|(left_class, left), (right_class, right)| {
                right.cmp(left).then(left_class.cmp(right_class))
            });
        slice.failures.truncate(FAILURE_LIMIT);
    }
    by_tool.sort_by(|left, right| {
        right
            .calls
            .cmp(&left.calls)
            .then(left.tool.cmp(&right.tool))
    });
    stats.by_tool = by_tool;
    stats
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        FAILURE_LIMIT, Latency, P95, ToolCall, ToolLedger, ToolOutcome, ToolStats, bucket_ceiling,
        bucket_floor, bucket_for, bucket_of, summarize,
    };
    use crate::StateDir;
    use crate::sessions::{StoredToolUsage, ToolBucket};

    const HOUR: u64 = 60 * 60;
    const READ: &str = "file_read";
    const SHELL: &str = "shell";
    const NATIVE: &str = "native";
    const PROJECT: &str = "/work/one";
    const OTHER_PROJECT: &str = "/work/two";
    /// A percentile reports the top of the bucket the call landed in, so the
    /// expected value is the ceiling of a duration rather than the duration.
    const P95_OF_5_MS: u64 = 5;
    const P95_OF_20_MS: u64 = 21;
    const P95_OF_30_MS: u64 = 31;
    const P95_OF_900_MS: u64 = 959;
    const TRUNCATED_PAIR: &[u8] = &[1];
    const DECODE_FIELD: &str = "test.latency";

    fn dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, dir)
    }

    fn call(
        tool: &str,
        cwd: &str,
        outcome: ToolOutcome,
        duration_ms: u64,
        tokens: u32,
    ) -> ToolCall {
        ToolCall {
            tool: tool.to_owned(),
            source: NATIVE.to_owned(),
            cwd: cwd.to_owned(),
            outcome,
            duration_ms,
            tokens,
        }
    }

    fn bucket(outcome: ToolOutcome, calls: u64) -> ToolBucket {
        ToolBucket {
            bucket_start: 0,
            tool: READ.to_owned(),
            source: NATIVE.to_owned(),
            cwd: PROJECT.to_owned(),
            outcome,
            calls,
            duration_ms: calls * 10,
            tokens: calls * 100,
            latency: latency(&vec![10; calls as usize]),
        }
    }

    fn latency(durations: &[u64]) -> Latency {
        let mut latency = Latency::default();
        for duration in durations {
            latency.record(*duration);
        }
        latency
    }

    #[test_case(0; "zero")]
    #[test_case(1; "one")]
    #[test_case(7; "last exact millisecond")]
    #[test_case(8; "first bucketed millisecond")]
    #[test_case(999; "sub second")]
    #[test_case(60_000; "a minute")]
    #[test_case(u32::MAX as u64; "far out")]
    fn a_duration_lands_in_a_bucket_that_holds_it(duration_ms: u64) {
        let bucket = bucket_of(duration_ms);
        assert!(bucket_floor(bucket) <= duration_ms);
        assert!(duration_ms <= bucket_ceiling(bucket));
    }

    #[test]
    fn a_histogram_survives_the_round_trip_through_bytes() {
        let recorded = latency(&[0, 3, 3, 40, 900, 900, 900, 60_000]);

        let decoded = Latency::decode(&recorded.encode(), DECODE_FIELD).unwrap();

        assert_eq!(decoded, recorded);
    }

    #[test]
    fn merging_adds_counts_rather_than_replacing_them() {
        let mut merged = latency(&[5, 5]);
        merged.merge(&latency(&[900]));

        assert_eq!(merged.percentile(0.5), Some(P95_OF_5_MS));
        assert_eq!(merged.percentile(1.0), Some(P95_OF_900_MS));
    }

    #[test]
    fn an_empty_histogram_has_no_percentile() {
        assert_eq!(Latency::default().percentile(P95), None);
    }

    #[test]
    fn a_truncated_pair_is_refused_rather_than_read_as_zero() {
        assert!(Latency::decode(TRUNCATED_PAIR, DECODE_FIELD).is_err());
    }

    #[test]
    fn repeated_calls_fold_into_one_bucket() {
        let (_temp, dir) = dir();
        let ledger = ToolLedger::open(&dir).unwrap();
        ledger
            .record_at(&call(READ, PROJECT, ToolOutcome::Ok, 10, 100), 0)
            .unwrap();
        ledger
            .record_at(&call(READ, PROJECT, ToolOutcome::Ok, 30, 200), HOUR / 2)
            .unwrap();

        let buckets = ledger.buckets(None, None).unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].calls, 2);
        assert_eq!(buckets[0].duration_ms, 40);
        assert_eq!(buckets[0].tokens, 300);
        assert_eq!(buckets[0].latency.percentile(1.0), Some(P95_OF_30_MS));
    }

    #[test]
    fn a_faster_call_never_hides_the_slow_one_behind_it() {
        let (_temp, dir) = dir();
        let ledger = ToolLedger::open(&dir).unwrap();
        ledger
            .record_at(&call(SHELL, PROJECT, ToolOutcome::Ok, 900, 0), 0)
            .unwrap();
        ledger
            .record_at(&call(SHELL, PROJECT, ToolOutcome::Ok, 5, 0), 0)
            .unwrap();

        let buckets = ledger.buckets(None, None).unwrap();
        assert_eq!(buckets[0].latency.percentile(1.0), Some(P95_OF_900_MS));
        assert_eq!(buckets[0].latency.percentile(0.5), Some(P95_OF_5_MS));
    }

    #[test]
    fn outcomes_are_recorded_apart_and_hours_are_not() {
        let (_temp, dir) = dir();
        let ledger = ToolLedger::open(&dir).unwrap();
        ledger
            .record_at(&call(SHELL, PROJECT, ToolOutcome::Ok, 10, 0), 0)
            .unwrap();
        ledger
            .record_at(&call(SHELL, PROJECT, ToolOutcome::Timeout, 10, 0), 0)
            .unwrap();
        ledger
            .record_at(&call(SHELL, PROJECT, ToolOutcome::Timeout, 10, 0), HOUR)
            .unwrap();

        assert_eq!(ledger.buckets(None, None).unwrap().len(), 3);
        let stats = ledger.lifetime().unwrap();
        assert_eq!(stats.calls, 3);
        assert_eq!(stats.errors, 2);
        assert_eq!(stats.by_tool[0].failures, vec![(ToolOutcome::Timeout, 2)]);
    }

    #[test_case(ToolOutcome::Ok; "ok")]
    #[test_case(ToolOutcome::Cancelled; "cancelled")]
    #[test_case(ToolOutcome::Timeout; "timeout")]
    #[test_case(ToolOutcome::Denied; "denied")]
    #[test_case(ToolOutcome::NotFound; "not_found")]
    #[test_case(ToolOutcome::InvalidInput; "invalid_input")]
    #[test_case(ToolOutcome::Other; "other")]
    fn every_outcome_round_trips_through_storage(outcome: ToolOutcome) {
        let (_temp, dir) = dir();
        let ledger = ToolLedger::open(&dir).unwrap();
        ledger
            .record_at(&call(READ, PROJECT, outcome, 1, 1), 0)
            .unwrap();

        assert_eq!(ledger.buckets(None, None).unwrap()[0].outcome, outcome);
        assert_eq!(
            ToolOutcome::from_storage_name(outcome.storage_name()),
            Some(outcome)
        );
    }

    #[test]
    fn a_project_sees_only_its_own_calls() {
        let (_temp, dir) = dir();
        let ledger = ToolLedger::open(&dir).unwrap();
        ledger
            .record_at(&call(READ, PROJECT, ToolOutcome::Ok, 10, 100), 0)
            .unwrap();
        ledger
            .record_at(&call(READ, OTHER_PROJECT, ToolOutcome::Ok, 10, 100), 0)
            .unwrap();

        assert_eq!(ledger.project(PROJECT).unwrap().calls, 1);
        assert_eq!(ledger.lifetime().unwrap().calls, 2);
    }

    #[test]
    fn pruning_drops_only_buckets_that_start_before_the_cutoff() {
        let (_temp, dir) = dir();
        let ledger = ToolLedger::open(&dir).unwrap();
        ledger
            .record_at(&call(READ, PROJECT, ToolOutcome::Ok, 1, 1), 0)
            .unwrap();
        ledger
            .record_at(&call(READ, PROJECT, ToolOutcome::Ok, 1, 1), HOUR * 2)
            .unwrap();

        assert_eq!(ledger.prune_before(bucket_for(HOUR * 2)).unwrap(), 1);
        assert_eq!(ledger.buckets(None, None).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_ledger_is_empty_rather_than_an_error() {
        let (_temp, dir) = dir();
        let stats = ToolLedger::open(&dir).unwrap().lifetime().unwrap();
        assert!(stats.is_empty());
        assert!(stats.by_tool.is_empty());
    }

    #[test]
    fn tools_rank_by_calls_and_shares_are_taken_against_the_totals() {
        let stats = summarize(&[
            ToolBucket {
                tool: SHELL.to_owned(),
                calls: 3,
                duration_ms: 300,
                tokens: 30,
                ..bucket(ToolOutcome::Ok, 3)
            },
            bucket(ToolOutcome::Ok, 7),
        ]);

        assert_eq!(stats.calls, 10);
        assert_eq!(stats.duration_ms, 370);
        assert_eq!(stats.by_tool[0].tool, READ);
        assert_eq!(stats.by_tool[0].calls, 7);
        assert_eq!(stats.by_tool[1].tool, SHELL);
    }

    #[test]
    fn failure_classes_rank_by_count_and_are_capped() {
        let classes = [
            ToolOutcome::Cancelled,
            ToolOutcome::Timeout,
            ToolOutcome::Denied,
            ToolOutcome::NotFound,
            ToolOutcome::InvalidInput,
            ToolOutcome::Other,
        ];
        let buckets: Vec<ToolBucket> = classes
            .iter()
            .enumerate()
            .map(|(index, class)| bucket(*class, index as u64 + 1))
            .collect();

        let stats = summarize(&buckets);
        let slice = &stats.by_tool[0];
        assert_eq!(slice.errors, 21);
        assert_eq!(slice.failures.len(), FAILURE_LIMIT);
        assert_eq!(slice.failures[0], (ToolOutcome::Other, 6));
        assert_eq!(slice.error_rate(), Some(1.0));
    }

    #[test]
    fn a_never_called_tool_has_no_rate_to_report() {
        let stats = ToolStats::from_session(&[]);
        assert!(stats.by_tool.is_empty());
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn session_rows_summarize_the_way_buckets_do() {
        let stats = ToolStats::from_session(&[
            StoredToolUsage {
                tool: READ.to_owned(),
                source: NATIVE.to_owned(),
                outcome: ToolOutcome::Ok,
                calls: 4,
                duration_ms: 80,
                tokens: 400,
                latency: latency(&[20, 20, 20, 20]),
            },
            StoredToolUsage {
                tool: READ.to_owned(),
                source: NATIVE.to_owned(),
                outcome: ToolOutcome::NotFound,
                calls: 1,
                duration_ms: 5,
                tokens: 10,
                latency: latency(&[5]),
            },
        ]);

        assert_eq!(stats.calls, 5);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.by_tool[0].p95_duration_ms(), Some(P95_OF_20_MS));
        assert_eq!(stats.by_tool[0].mean_duration_ms(), Some(17));
        assert_eq!(stats.by_tool[0].error_rate(), Some(0.2));
    }
}
