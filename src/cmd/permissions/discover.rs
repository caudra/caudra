use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use caudra_agent::permissions::canonical_json_sha256;
use caudra_agent::permissions::pattern_matching::CompiledPattern;
use caudra_agent::permissions::pattern_recognition::{
    CommandObservation, InvocationOutcome, MAX_RECOGNIZER_BYTES, MAX_RECOGNIZER_OBSERVATIONS,
    MAX_RECOGNIZER_SUGGESTIONS, MAX_TIMESTAMP_MS, ObservationProvenance, ObserveOutcome,
    PatternCandidate, PatternRecognizer, RecognitionExclusion, RecognitionStats, RecognizerLimits,
};
use caudra_agent::permissions::review::slot_label;
use caudra_agent::tools::native::batch::MAX_BATCH_SIZE;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::permission_patterns::{ArgumentDomain, PatternToken, SlotCombinations};
use caudra_storage::sessions::{
    HistoryReadLimits, HistoryReadReport, HistoryRecord, SESSIONS_DB_FILE, SessionDatabase,
};
use caudra_workcell::{
    BashContextAssumptions, PatternCallDiagnostics, PatternObligationCount, PatternOmissionReason,
    analyze_pattern_calls,
};
use color_eyre::eyre::{Result, bail, eyre};
use jiff::Timestamp;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

const REPORT_VERSION: u16 = 1;
const DEFAULT_SUGGESTIONS: usize = 10;
const MAX_SESSIONS: usize = 256;
const MAX_ROWS: usize = 10_000;
const MAX_ROWS_PER_SESSION: usize = 128;
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ROW_BYTES: usize = 256 * 1024;
const MAX_CALLS: usize = 4096;
const MAX_COMMAND_BYTES: usize = 8 * 1024;
const MAX_ANALYSIS_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_ELAPSED_MS: u64 = 2000;
const MAX_ELAPSED_MS: u64 = 10_000;
const SAMPLING_BUDGET_DIVISOR: u32 = 2;
const REPORT_BUDGET_DIVISOR: u32 = 4;
const MAX_PATH_BYTES: usize = 4096;
const MAX_TIMESTAMP_BYTES: usize = 64;
const MAX_CALL_ID_BYTES: usize = 256;
const MAX_DISPLAY_BYTES: usize = 240;
const NANOS_PER_MILLISECOND: i128 = 1_000_000;
const TOOL_RESULT_RECORD: &str = "tool_result";
const REFUSED_CALLS_FIELD: &str = "refused_calls";
const INVALID_PROJECT: &str = "discovery requires a bounded absolute UTF-8 project path without parent components or control characters";
const INVALID_SINCE: &str =
    "--since must be an RFC3339 timestamp between the Unix epoch and year 9999";
pub(crate) const RECOGNIZER_CAPACITY: &str = "Recognizer capacity exclusions";
const REFUSED_CALLS_SKIPPED: &str = "Calls refused before they ran, skipped";
pub(crate) const RECOGNIZER_ORDER_BIAS: &str = "Recognizer admission is first-come and capacity-bounded; changing input order can change retained evidence and proposals.";
const ASSUMPTIONS: &[&str] = &[
    "Imported proposals only: historical execution context is NOT verified; outcomes are unknown.",
    "Declared standard Bash: startup preserves cwd; no aliases, functions, command-not-found hook or traps; default shell options and standard builtins.",
    "Directory variables are standard, CDPATH is empty, lastpipe is disabled, and logical PWD matches the initial cwd.",
    "The session's CURRENT stored cwd approximates its historical project; explicit workdir and cd are interpreted lexically, without filesystem resolution.",
];
const LIMITATIONS: &[&str] = &[
    "Read-only discovery never executes history, installs rules, grants authority or changes runtime authorization.",
    "Only structurally stored tool_call records named shell/functions.shell or native batch are eligible; names do not authenticate historical tool identity. MCP, unknown tools, text and outputs are excluded.",
    "Dates are validated base58 UUIDv7 history creation times, not execution times. Missing/invalid/future dates are skipped, never replaced with session activity times.",
    "Recent sessions are sampled first, with a shared per-parent row cap across main history and subagents in reverse indexed order. Limits include duplicate, invalid and skipped rows; archives and deleted history are not scanned.",
    "A per-session cutoff skips the remaining rows and advances to the next parent. Per-session counts cover visited local parents only; this is a bounded prefix sample, not representative coverage or a count of all omitted rows.",
    "Global byte, call and time budgets can still stop sampling before a parent reaches its row cap.",
    "Repeated history IDs (including copied/forked history) count once. Conflicting payloads quarantine the whole record. Subagents do not count as independent parent sessions.",
    "Unsupported, dynamic, sensitive-looking and interpreted-payload inputs are omitted. Conservative screening cannot establish that arbitrary historical literals contain no secrets; review JSON before sharing it.",
    "Recognizer context_verified metadata means analysis succeeded under the declared assumptions, NOT that historical context was proven. Imported evidence never becomes native evidence.",
    "Observed commands and matching proposals are not full-call authorization: omitted scopes, source/operator obligations and unassessed program effects remain. Diagnostics count analyzed calls, not verified executions.",
    "Diagnostic totals precede history quarantine and bounded recognizer admission; observed-command counts are not retained proposal support.",
    RECOGNIZER_ORDER_BIAS,
    "Time/cancellation checks run between bounded rows, calls, parser and recognizer operations; a single in-flight bounded operation is not preempted. Storage locks are nonblocking.",
    "Sampling stops at half the elapsed budget and observation admission at three quarters, reserving the final quarter for suggestions and evidence. Phase cutoffs retain partial results; only proposals with complete admitted evidence are returned. Cancellation discards all proposals.",
    "Imported proposals still require at least two observations from two independent parent sessions; a bounded single-session sample cannot establish that support.",
    "Partial reads and exhausted limits describe only the sampled prefixes; duplicates or collisions beyond a cutoff cannot be detected. Read-only SQLite can update existing WAL coordination sidecars, but not logical database state.",
];

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryLimits {
    pub max_suggestions: usize,
    pub since_ms: Option<u64>,
    pub history: HistoryReadLimits,
    pub max_rows_per_session: usize,
    pub max_calls: usize,
    pub max_analysis_bytes: usize,
    pub max_elapsed_ms: u64,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_suggestions: DEFAULT_SUGGESTIONS,
            since_ms: None,
            history: HistoryReadLimits {
                max_sessions: MAX_SESSIONS,
                max_rows: MAX_ROWS,
                max_bytes: MAX_HISTORY_BYTES,
                max_row_bytes: MAX_ROW_BYTES,
            },
            max_rows_per_session: MAX_ROWS_PER_SESSION,
            max_calls: MAX_CALLS,
            max_analysis_bytes: MAX_ANALYSIS_BYTES,
            max_elapsed_ms: DEFAULT_ELAPSED_MS,
        }
    }
}

impl DiscoveryLimits {
    fn bounded(mut self) -> Result<Self> {
        if self.since_ms.is_some_and(|since| since > MAX_TIMESTAMP_MS) {
            bail!(INVALID_SINCE);
        }
        self.max_suggestions = self.max_suggestions.clamp(1, MAX_RECOGNIZER_SUGGESTIONS);
        self.history.max_sessions = self.history.max_sessions.min(MAX_SESSIONS);
        self.history.max_rows = self.history.max_rows.min(MAX_ROWS);
        self.history.max_bytes = self.history.max_bytes.min(MAX_HISTORY_BYTES);
        self.history.max_row_bytes = self.history.max_row_bytes.min(MAX_ROW_BYTES);
        self.max_rows_per_session = self.max_rows_per_session.min(MAX_ROWS_PER_SESSION);
        self.max_calls = self.max_calls.min(MAX_CALLS);
        self.max_analysis_bytes = self.max_analysis_bytes.min(MAX_ANALYSIS_BYTES);
        self.max_elapsed_ms = self.max_elapsed_ms.min(MAX_ELAPSED_MS);
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryExclusion {
    BeforeSince,
    FutureTimestamp,
    NonToolRecord,
    UnsupportedTool,
    MalformedCall,
    InvalidWorkdir,
    CommandSize,
    UnsupportedOrSensitiveAnalysis,
    RefusedCall,
}

/// A call as its stored result names it: the session, the subagent stream,
/// and the call ID.
type CallKey = (CaudraId, Option<String>, String);

#[derive(Debug, Default, Serialize)]
pub struct DiscoveryStats {
    pub calls: usize,
    pub shell_calls: usize,
    pub analyzed_shell_calls: usize,
    pub analysis_failures: usize,
    pub analysis_bytes: usize,
    pub represented_commands: usize,
    pub observed_commands: usize,
    pub calls_with_omitted_commands: usize,
    pub calls_with_incomplete_source: usize,
    pub calls_with_incomplete_context: usize,
    pub omission_counts: BTreeMap<PatternOmissionReason, usize>,
    pub obligation_counts: Vec<PatternObligationCount>,
    pub duplicate_records: usize,
    pub colliding_records: usize,
    pub quarantined_records: usize,
    pub exclusions: BTreeMap<DiscoveryExclusion, usize>,
    pub call_limit: bool,
    pub analysis_byte_limit: bool,
    pub observation_limit: bool,
    pub sampling_time_limit: bool,
    pub recognition_time_limit: bool,
    pub storage_unavailable: bool,
    pub recognition_failed: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl DiscoveryStats {
    fn exclude(&mut self, reason: DiscoveryExclusion) {
        *self.exclusions.entry(reason).or_default() += 1;
    }

    fn record_diagnostics(&mut self, diagnostics: &PatternCallDiagnostics) {
        self.analyzed_shell_calls += 1;
        self.represented_commands += diagnostics.represented_command_count;
        self.observed_commands += diagnostics.observed_command_count;
        self.calls_with_omitted_commands +=
            usize::from(diagnostics.observed_command_count < diagnostics.represented_command_count);
        self.calls_with_incomplete_source +=
            usize::from(!diagnostics.obligations.source_coverage_complete);
        self.calls_with_incomplete_context +=
            usize::from(!diagnostics.obligations.context_complete);
        for (reason, count) in &diagnostics.omission_counts {
            *self.omission_counts.entry(reason.clone()).or_default() += count;
        }
        for obligation in &diagnostics.obligations.counts {
            if let Some(total) = self
                .obligation_counts
                .iter_mut()
                .find(|total| total.kind == obligation.kind)
            {
                total.count += obligation.count;
            } else {
                self.obligation_counts.push(obligation.clone());
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEvidence {
    pub session_id: CaudraId,
    pub history_id: CaudraId,
    pub timestamp_ms: u64,
    pub ordinal: u64,
    pub subagent_id_hash: Option<String>,
    pub observation_id: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryReport {
    pub version: u16,
    pub read_only: bool,
    pub historical_context_verified: bool,
    pub project: PathBuf,
    pub source_identity: String,
    pub limits: DiscoveryLimits,
    #[serde(serialize_with = "serialize_recognizer_limits")]
    pub recognizer_limits: RecognizerLimits,
    pub max_command_bytes: usize,
    pub as_of_ms: u64,
    pub sample: HistoryReadReport,
    pub processing: DiscoveryStats,
    pub recognition: RecognitionStats,
    pub candidates: Vec<PatternCandidate>,
    /// Keyed by definition fingerprint; no raw call IDs, command text or outputs.
    pub provenance: BTreeMap<String, Vec<HistoryEvidence>>,
    pub assumptions: &'static [&'static str],
    pub limitations: &'static [&'static str],
}

struct AnalyzedObservation {
    observation: CommandObservation,
    history: HistoryEvidence,
}

struct SeenRecord {
    fingerprint: String,
    observations: Vec<AnalyzedObservation>,
    quarantined: bool,
}

enum DiscoveryPhase {
    Sampling,
    Recognition,
    Reporting,
}

impl DiscoveryPhase {
    fn deadline(&self, budget: Duration) -> Duration {
        match self {
            Self::Sampling => budget / SAMPLING_BUDGET_DIVISOR,
            Self::Recognition => budget - budget / REPORT_BUDGET_DIVISOR,
            Self::Reporting => budget,
        }
    }
}

/// Blocking read-only work for a dedicated startup job, never for a UI/storage
/// actor lock. Only owned proposals cross back to the caller; no rules are saved.
pub fn discover_for_project(
    state_dir: &StateDir,
    project: &Path,
    limits: DiscoveryLimits,
) -> Result<DiscoveryReport> {
    discover_for_project_cancellable(state_dir, project, limits, || false)
}

pub fn discover_for_project_cancellable(
    state_dir: &StateDir,
    project: &Path,
    limits: DiscoveryLimits,
    cancelled: impl Fn() -> bool,
) -> Result<DiscoveryReport> {
    let started = Instant::now();
    let limits = limits.bounded()?;
    let budget = Duration::from_millis(limits.max_elapsed_ms);
    discover_with_budget(state_dir, project, limits, cancelled, |phase| {
        started.elapsed() >= phase.deadline(budget)
    })
}

fn discover_with_budget(
    state_dir: &StateDir,
    project: &Path,
    limits: DiscoveryLimits,
    cancelled: impl Fn() -> bool,
    expired: impl Fn(DiscoveryPhase) -> bool,
) -> Result<DiscoveryReport> {
    let project_text = project
        .to_str()
        .filter(|_| valid_absolute_path(project))
        .ok_or_else(|| eyre!(INVALID_PROJECT))?;
    let as_of_ms = u64::try_from(Timestamp::now().as_millisecond())
        .map_err(|_| eyre!("discovery clock is before the Unix epoch"))?;
    let source_identity = format!(
        "sqlite-history:{}",
        canonical_json_sha256(&json!(
            state_dir
                .path()
                .join(SESSIONS_DB_FILE)
                .as_os_str()
                .as_encoded_bytes()
        ))
    );
    let mut report = DiscoveryReport {
        version: REPORT_VERSION,
        read_only: true,
        historical_context_verified: false,
        project: project.to_path_buf(),
        source_identity,
        recognizer_limits: RecognizerLimits {
            max_suggestions: limits.max_suggestions,
            ..RecognizerLimits::default()
        },
        limits,
        max_command_bytes: MAX_COMMAND_BYTES,
        as_of_ms,
        sample: HistoryReadReport::default(),
        processing: DiscoveryStats::default(),
        recognition: RecognitionStats::default(),
        candidates: Vec::new(),
        provenance: BTreeMap::new(),
        assumptions: ASSUMPTIONS,
        limitations: LIMITATIONS,
    };
    let was_cancelled = Cell::new(false);
    let sampling_time_limit = Cell::new(false);
    let recognition_time_limit = Cell::new(false);
    let timed_out = Cell::new(false);
    let should_stop = |phase| {
        was_cancelled.set(was_cancelled.get() || cancelled());
        if was_cancelled.get() {
            return true;
        }
        let reached = match phase {
            DiscoveryPhase::Sampling => &sampling_time_limit,
            DiscoveryPhase::Recognition => &recognition_time_limit,
            DiscoveryPhase::Reporting => &timed_out,
        };
        reached.set(reached.get() || expired(phase));
        reached.get()
    };
    let stop_sampling = || should_stop(DiscoveryPhase::Sampling);
    let mut records = BTreeMap::<_, SeenRecord>::new();
    let mut refusals = BTreeMap::new();
    let mut retained_count = 0;
    let mut retained_bytes = 0;
    if !stop_sampling() {
        match SessionDatabase::open_read_only_nonblocking(state_dir) {
            Ok(database) => {
                let scan = database.visit_history_records_with_session_limit(
                    project_text,
                    &report.limits.history,
                    Some(report.limits.max_rows_per_session),
                    &mut report.sample,
                    stop_sampling,
                    |record| {
                        if let Some((call, positions)) = refused_calls(&record) {
                            refusals.insert(call, positions);
                        }
                        let fingerprint = canonical_json_sha256(record.payload);
                        let key = *record.history_id.as_bytes();
                        if let Some(previous) = records.get_mut(&key) {
                            if previous.quarantined {
                                report.processing.quarantined_records += 1;
                            } else if previous.fingerprint == fingerprint {
                                report.processing.duplicate_records += 1;
                            } else {
                                previous.observations.clear();
                                previous.quarantined = true;
                                report.processing.colliding_records += 1;
                            }
                            return ControlFlow::Continue(());
                        }
                        let mut observations = Vec::new();
                        if record.timestamp_ms > as_of_ms {
                            report
                                .processing
                                .exclude(DiscoveryExclusion::FutureTimestamp);
                        } else if report
                            .limits
                            .since_ms
                            .is_some_and(|since| record.timestamp_ms < since)
                        {
                            report.processing.exclude(DiscoveryExclusion::BeforeSince);
                        } else {
                            analyze_record(
                                &record,
                                project,
                                &report.limits,
                                &mut report.processing,
                                &stop_sampling,
                                &refusals,
                                &mut |mut observation, call_index, command_index| {
                                    observation.source.source_identity =
                                        report.source_identity.clone();
                                    observation.source.observation_id = format!(
                                        "{}:{call_index}:{command_index}",
                                        record.history_id
                                    );
                                    observation.source.session_id = record.session_id.to_string();
                                    observation.source.timestamp_ms = record.timestamp_ms;
                                    observation.source.provenance = ObservationProvenance::Imported;
                                    observation.source.outcome = InvocationOutcome::Unknown;
                                    let bytes = serde_json::to_vec(&observation)
                                        .map_or(MAX_RECOGNIZER_BYTES, |value| value.len());
                                    if retained_count >= MAX_RECOGNIZER_OBSERVATIONS
                                        || bytes
                                            > MAX_RECOGNIZER_BYTES.saturating_sub(retained_bytes)
                                    {
                                        return false;
                                    }
                                    retained_count += 1;
                                    retained_bytes += bytes;
                                    observations.push(AnalyzedObservation {
                                        history: HistoryEvidence {
                                            session_id: record.session_id,
                                            history_id: record.history_id,
                                            timestamp_ms: record.timestamp_ms,
                                            ordinal: record.ordinal,
                                            subagent_id_hash: record
                                                .subagent_id
                                                .map(|id| canonical_json_sha256(&json!(id))),
                                            observation_id: observation
                                                .source
                                                .observation_id
                                                .clone(),
                                        },
                                        observation,
                                    });
                                    true
                                },
                            );
                        }
                        records.insert(
                            key,
                            SeenRecord {
                                fingerprint,
                                observations,
                                quarantined: false,
                            },
                        );
                        if report.processing.call_limit
                            || report.processing.analysis_byte_limit
                            || report.processing.observation_limit
                        {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                );
                report.processing.storage_unavailable = scan.is_err();
            }
            Err(_) => report.processing.storage_unavailable = true,
        }
    }
    let mut recognizer = PatternRecognizer::new(report.recognizer_limits.clone(), as_of_ms)?;
    let mut accepted = Vec::new();
    for record in records.into_values() {
        for row in record.observations {
            if should_stop(DiscoveryPhase::Recognition) {
                break;
            }
            if matches!(
                recognizer.observe(row.observation.clone()),
                Ok(ObserveOutcome::Added)
            ) {
                accepted.push(row);
            }
        }
        if should_stop(DiscoveryPhase::Recognition) {
            break;
        }
    }
    report_candidates(&mut report, &recognizer, accepted, &should_stop);
    should_stop(DiscoveryPhase::Reporting);
    report.processing.sampling_time_limit = sampling_time_limit.get();
    report.processing.recognition_time_limit = recognition_time_limit.get();
    report.processing.cancelled = was_cancelled.get();
    report.processing.timed_out = timed_out.get();
    if report.processing.cancelled {
        report.candidates.clear();
        report.provenance.clear();
    }
    Ok(report)
}

fn report_candidates(
    report: &mut DiscoveryReport,
    recognizer: &PatternRecognizer,
    mut accepted: Vec<AnalyzedObservation>,
    should_stop: &impl Fn(DiscoveryPhase) -> bool,
) {
    accepted.retain(|row| {
        !should_stop(DiscoveryPhase::Reporting) && recognizer.retains(&row.observation)
    });
    report.recognition = recognizer.stats();
    if !should_stop(DiscoveryPhase::Reporting) {
        match recognizer.suggestions() {
            Ok(candidates) => {
                for candidate in candidates {
                    if should_stop(DiscoveryPhase::Reporting) {
                        break;
                    }
                    let Ok(compiled) = CompiledPattern::compile(&candidate.definition) else {
                        report.processing.recognition_failed = true;
                        continue;
                    };
                    let mut history = Vec::new();
                    for row in &accepted {
                        if should_stop(DiscoveryPhase::Reporting) {
                            break;
                        }
                        match compiled.matches(&row.observation) {
                            Ok(result) if result.is_match() => history.push(row.history.clone()),
                            Err(_) => report.processing.recognition_failed = true,
                            _ => {}
                        }
                    }
                    if should_stop(DiscoveryPhase::Reporting) {
                        break;
                    }
                    report
                        .provenance
                        .insert(compiled.fingerprint().into(), history);
                    report.candidates.push(candidate);
                }
            }
            Err(_) => report.processing.recognition_failed = true,
        }
    }
}

fn analyze_record(
    record: &HistoryRecord<'_>,
    project: &Path,
    limits: &DiscoveryLimits,
    stats: &mut DiscoveryStats,
    should_stop: &impl Fn() -> bool,
    refusals: &BTreeMap<CallKey, Vec<usize>>,
    observe: &mut impl FnMut(CommandObservation, usize, usize) -> bool,
) {
    if record.payload.get("type").and_then(Value::as_str) != Some("tool_call") {
        stats.exclude(DiscoveryExclusion::NonToolRecord);
        return;
    }
    let Some(name) = record.payload.get("name").and_then(Value::as_str) else {
        stats.exclude(DiscoveryExclusion::MalformedCall);
        return;
    };
    let Some(call_id) = stored_call_id(record.payload) else {
        stats.exclude(DiscoveryExclusion::MalformedCall);
        return;
    };
    let Some(input) = record.payload.get("input").and_then(Value::as_object) else {
        stats.exclude(DiscoveryExclusion::MalformedCall);
        return;
    };
    let refused = refusals
        .get(&call_key(record, call_id))
        .map_or(&[][..], Vec::as_slice);
    if native_name(name) == "batch" {
        let Some(calls) = input
            .get("tool_calls")
            .and_then(Value::as_array)
            .filter(|calls| !calls.is_empty() && calls.len() <= MAX_BATCH_SIZE)
        else {
            stats.exclude(DiscoveryExclusion::MalformedCall);
            return;
        };
        for (index, entry) in calls.iter().enumerate() {
            if should_stop() {
                return;
            }
            if stats.calls >= limits.max_calls {
                stats.call_limit = true;
                return;
            }
            stats.calls += 1;
            if refused.contains(&index) {
                stats.exclude(DiscoveryExclusion::RefusedCall);
                continue;
            }
            let Some((name, input)) = batch_call(entry) else {
                stats.exclude(DiscoveryExclusion::MalformedCall);
                continue;
            };
            analyze_call(
                name,
                &input,
                project,
                limits,
                stats,
                should_stop,
                &mut |observation, command| observe(observation, index, command),
            );
            if stats.analysis_byte_limit || stats.observation_limit {
                return;
            }
        }
    } else {
        if stats.calls >= limits.max_calls {
            stats.call_limit = true;
            return;
        }
        stats.calls += 1;
        if refused.contains(&0) {
            stats.exclude(DiscoveryExclusion::RefusedCall);
            return;
        }
        analyze_call(
            name,
            input,
            project,
            limits,
            stats,
            should_stop,
            &mut |observation, command| observe(observation, 0, command),
        );
    }
}

fn stored_call_id(payload: &Value) -> Option<&str> {
    payload.get("call_id").and_then(Value::as_str).filter(|id| {
        !id.is_empty() && id.len() <= MAX_CALL_ID_BYTES && !id.chars().any(char::is_control)
    })
}

fn call_key(record: &HistoryRecord<'_>, call_id: &str) -> CallKey {
    (
        record.session_id,
        record.subagent_id.map(str::to_owned),
        call_id.to_owned(),
    )
}

/// The calls a stored result says were refused before they ran. Storage
/// visits a stream newest first, so a call's result arrives before the call.
fn refused_calls(record: &HistoryRecord<'_>) -> Option<(CallKey, Vec<usize>)> {
    if record.payload.get("type").and_then(Value::as_str) != Some(TOOL_RESULT_RECORD) {
        return None;
    }
    let call_id = stored_call_id(record.payload)?;
    let positions: Vec<usize> = record
        .payload
        .get(REFUSED_CALLS_FIELD)?
        .as_array()?
        .iter()
        .take(MAX_BATCH_SIZE)
        .filter_map(Value::as_u64)
        .filter_map(|position| usize::try_from(position).ok())
        .filter(|position| *position < MAX_BATCH_SIZE)
        .collect();
    (!positions.is_empty()).then(|| (call_key(record, call_id), positions))
}

fn native_name(name: &str) -> &str {
    name.strip_prefix("functions.").unwrap_or(name)
}

fn batch_call(entry: &Value) -> Option<(&str, Map<String, Value>)> {
    let entry = entry.as_object()?;
    let name = entry.get("tool")?.as_str()?;
    let mut input: Map<_, _> = entry
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "tool" | "parameters"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(parameters) = entry.get("parameters") {
        for (key, value) in parameters.as_object()? {
            if input.insert(key.clone(), value.clone()).is_some() {
                return None;
            }
        }
    }
    Some((name, input))
}

fn analyze_call(
    name: &str,
    input: &Map<String, Value>,
    project: &Path,
    limits: &DiscoveryLimits,
    stats: &mut DiscoveryStats,
    should_stop: &impl Fn() -> bool,
    observe: &mut impl FnMut(CommandObservation, usize) -> bool,
) {
    if should_stop() {
        return;
    }
    if native_name(name) != "shell" {
        stats.exclude(DiscoveryExclusion::UnsupportedTool);
        return;
    }
    stats.shell_calls += 1;
    // Sessions recorded before the deadline moved to seconds name it in
    // milliseconds as `timeout`, and discovery reads history as written.
    if input.iter().any(|(key, value)| match key.as_str() {
        "command" | "workdir" => false,
        "timeoutSec" | "timeout" => value.as_u64().is_none(),
        _ => true,
    }) {
        stats.exclude(DiscoveryExclusion::MalformedCall);
        return;
    }
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        stats.exclude(DiscoveryExclusion::MalformedCall);
        return;
    };
    if command.is_empty() || command.len() > MAX_COMMAND_BYTES {
        stats.exclude(DiscoveryExclusion::CommandSize);
        return;
    }
    if command.len()
        > limits
            .max_analysis_bytes
            .saturating_sub(stats.analysis_bytes)
    {
        stats.analysis_byte_limit = true;
        return;
    }
    let workdir = match input.get("workdir") {
        None => project.to_path_buf(),
        Some(Value::String(value)) if !value.is_empty() => project.join(value),
        _ => {
            stats.exclude(DiscoveryExclusion::InvalidWorkdir);
            return;
        }
    };
    if !valid_absolute_path(&workdir) {
        stats.exclude(DiscoveryExclusion::InvalidWorkdir);
        return;
    }
    let workdir = workdir.components().collect::<PathBuf>();
    stats.analysis_bytes += command.len();
    let Ok(analysis) =
        analyze_pattern_calls(command, &workdir, project, standard_bash_assumptions())
    else {
        stats.analysis_failures += 1;
        stats.exclude(DiscoveryExclusion::UnsupportedOrSensitiveAnalysis);
        return;
    };
    stats.record_diagnostics(&analysis.diagnostics);
    if analysis.requires_exact_source || analysis.observations.is_empty() {
        stats.exclude(DiscoveryExclusion::UnsupportedOrSensitiveAnalysis);
        return;
    }
    for (index, observation) in analysis.observations.into_iter().enumerate() {
        if should_stop() {
            return;
        }
        if !observe(observation, index) {
            stats.observation_limit = true;
            return;
        }
    }
}

fn standard_bash_assumptions() -> BashContextAssumptions {
    BashContextAssumptions {
        startup_preserves_cwd: true,
        no_aliases_functions_or_command_not_found_hook: true,
        no_traps: true,
        default_shell_options: true,
        standard_builtins: true,
        directory_variables_are_standard: true,
        cdpath_empty: true,
        lastpipe_disabled: true,
        logical_pwd_matches_initial: true,
    }
}

fn valid_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| text.len() <= MAX_PATH_BYTES && !text.chars().any(char::is_control))
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

fn parse_since(value: &str) -> Result<u64> {
    if value.len() > MAX_TIMESTAMP_BYTES {
        bail!(INVALID_SINCE);
    }
    let timestamp = value
        .parse::<Timestamp>()
        .map_err(|_| eyre!(INVALID_SINCE))?;
    let nanos = timestamp.as_nanosecond();
    if nanos < 0 {
        bail!(INVALID_SINCE);
    }
    u64::try_from((nanos + NANOS_PER_MILLISECOND - 1) / NANOS_PER_MILLISECOND)
        .ok()
        .filter(|value| *value <= MAX_TIMESTAMP_MS)
        .ok_or_else(|| eyre!(INVALID_SINCE))
}

fn serialize_recognizer_limits<S: Serializer>(
    limits: &RecognizerLimits,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    json!({
        "max_observations": limits.max_observations,
        "max_clusters": limits.max_clusters,
        "max_cluster_observations": limits.max_cluster_observations,
        "max_observation_bytes": limits.max_observation_bytes,
        "max_suggestions": limits.max_suggestions,
        "min_support": limits.min_support,
        "min_sessions": limits.min_sessions,
    })
    .serialize(serializer)
}

pub(super) fn run(
    state_dir: &StateDir,
    project: Option<PathBuf>,
    limit: Option<usize>,
    since: Option<String>,
    json: bool,
) -> Result<()> {
    let project = project.map(Ok).unwrap_or_else(std::env::current_dir)?;
    let report = discover_for_project(
        state_dir,
        &project,
        DiscoveryLimits {
            max_suggestions: limit.unwrap_or(DEFAULT_SUGGESTIONS),
            since_ms: since.as_deref().map(parse_since).transpose()?,
            ..DiscoveryLimits::default()
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }
    Ok(())
}

fn render_human(report: &DiscoveryReport) -> String {
    let mut output = String::from("Permission pattern proposals (read-only; nothing installed)\n");
    for assumption in report.assumptions {
        let _ = writeln!(output, "{assumption}");
    }
    let _ = writeln!(
        output,
        "Sample: {} sessions, {} rows, {} bytes; {} calls, {} shell calls, {} proposals.",
        report.sample.sessions,
        report.sample.rows,
        report.sample.bytes,
        report.processing.calls,
        report.processing.shell_calls,
        report.candidates.len()
    );
    let _ = writeln!(
        output,
        "{REFUSED_CALLS_SKIPPED}: {}. Results saved before refusals were recorded count as run.",
        report
            .processing
            .exclusions
            .get(&DiscoveryExclusion::RefusedCall)
            .copied()
            .unwrap_or_default()
    );
    let _ = writeln!(
        output,
        "Per-parent row cap: {}; {} sessions cut short. Local parent rows (newest first): {:?}.",
        report.limits.max_rows_per_session,
        report.sample.session_row_cutoffs,
        report
            .sample
            .per_session
            .iter()
            .map(|session| session.rows)
            .collect::<Vec<_>>()
    );
    let _ = writeln!(
        output,
        "Deduplication: {} repeated, {} colliding, {} quarantined records. Recognizer: {} retained, {} duplicate observations. {RECOGNIZER_CAPACITY}: {}.",
        report.processing.duplicate_records,
        report.processing.colliding_records,
        report.processing.quarantined_records,
        report.recognition.retained_observations,
        report.recognition.duplicates,
        report
            .recognition
            .exclusions
            .get(&RecognitionExclusion::Capacity)
            .copied()
            .unwrap_or_default()
    );
    let _ = writeln!(
        output,
        "Analysis: {} calls; {} observed / {} represented commands (not full-call authorization); {} calls with omitted commands, {} incomplete source, {} incomplete context, {} parse failures.",
        report.processing.analyzed_shell_calls,
        report.processing.observed_commands,
        report.processing.represented_commands,
        report.processing.calls_with_omitted_commands,
        report.processing.calls_with_incomplete_source,
        report.processing.calls_with_incomplete_context,
        report.processing.analysis_failures
    );
    let _ = writeln!(
        output,
        "Analysis omissions: {}. Source/effect obligations: {}.",
        serde_json::to_string(&report.processing.omission_counts).unwrap_or_default(),
        serde_json::to_string(&report.processing.obligation_counts).unwrap_or_default()
    );
    for (index, candidate) in report.candidates.iter().enumerate() {
        let definition = &candidate.definition;
        let mut shape = String::new();
        for token in &definition.argv {
            let atom = match token {
                PatternToken::Exact { value, .. } => {
                    serde_json::to_string(value).unwrap_or_default()
                }
                PatternToken::Slot { id, .. } => slot_label(definition, *id),
            };
            if shape.len() + atom.len() > MAX_DISPLAY_BYTES {
                shape.push_str(" [display clipped]");
                break;
            }
            if !shape.is_empty() {
                shape.push(' ');
            }
            shape.push_str(&atom);
        }
        let _ = writeln!(output, "\n{}. {shape}", index + 1);
        let combinations = match &definition.combinations {
            SlotCombinations::ObservedTuples { tuples } => format!(
                "observed tuples only ({}); no independent recombination",
                tuples.len()
            ),
            SlotCombinations::Independent => "exact argv; no variable slots".into(),
        };
        let _ = writeln!(output, "   Constraint mode: {combinations}.");
        for slot in &definition.slots {
            if let ArgumentDomain::ObservedSet { values } = &slot.domain {
                let _ = writeln!(
                    output,
                    "   {}: observed set ({} literal values); option-like arguments rejected.",
                    slot_label(definition, slot.id),
                    values.len()
                );
            }
        }
        let evidence = &candidate.evidence;
        let workdir =
            serde_json::to_string(&definition.context.effective_workdir).unwrap_or_default();
        let display_workdir: String = workdir.chars().take(MAX_DISPLAY_BYTES).collect();
        let _ = writeln!(
            output,
            "   Assumed cwd (approximate historical context): {display_workdir}"
        );
        let _ = writeln!(
            output,
            "   Imported support: {} observations / {} independent sessions; {} unknown outcomes; history dates {}–{}.",
            evidence.support.observations,
            evidence.support.independent_sessions,
            evidence
                .outcomes
                .get(&InvocationOutcome::Unknown)
                .copied()
                .unwrap_or_default(),
            format_timestamp(evidence.first_seen_ms),
            format_timestamp(evidence.last_seen_ms)
        );
    }
    let _ = writeln!(output, "\nLimits and exclusions: {}", serde_json::to_string(&json!({ "limits": report.limits, "sample": report.sample, "processing": report.processing, "recognition_exclusions": report.recognition.exclusions })).unwrap_or_default());
    for limitation in report.limitations {
        let _ = writeln!(output, "{limitation}");
    }
    output
}

fn format_timestamp(timestamp_ms: u64) -> String {
    i64::try_from(timestamp_ms)
        .ok()
        .and_then(|value| Timestamp::from_millisecond(value).ok())
        .map_or_else(|| "unknown".into(), |timestamp| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use caudra_agent::permissions::pattern_matching::CompiledPattern;
    use caudra_agent::permissions::pattern_recognition::{
        InvocationOutcome, ObservationProvenance, ObserveOutcome, PatternRecognizer,
        RecognitionError, RecognitionExclusion, RecognizerLimits,
    };
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use caudra_storage::permission_patterns::{ArgumentDomain, OptionLikePolicy, SlotCombinations};
    use caudra_storage::sessions::{SESSIONS_DB_FILE, Session, SessionDatabase, TitleSource};
    use caudra_workcell::{
        BashOperatorKind, PatternObligationKind, PatternOmissionReason, analyze_pattern_calls,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        AnalyzedObservation, DiscoveryExclusion, DiscoveryLimits, DiscoveryPhase, DiscoveryReport,
        HistoryEvidence, INVALID_PROJECT, INVALID_SINCE, MAX_COMMAND_BYTES, MAX_ELAPSED_MS,
        MAX_RECOGNIZER_SUGGESTIONS, MAX_ROWS_PER_SESSION, MAX_TIMESTAMP_MS, RECOGNIZER_CAPACITY,
        RECOGNIZER_ORDER_BIAS, REFUSED_CALLS_SKIPPED, SAMPLING_BUDGET_DIVISOR,
        discover_for_project, discover_for_project_cancellable, discover_with_budget, parse_since,
        render_human, report_candidates, standard_bash_assumptions,
    };

    const PROJECT: &str = "/nonexistent/historical-discovery-project";
    const OTHER_PROJECT: &str = "/nonexistent/other-discovery-project";
    const MODEL: &str = "test/model";
    const DATE: &str = "2026-01-01T00:00:00Z";
    const TIME_MS: u64 = 1_767_225_600_000;
    const UUID_TIME_SHIFT: u32 = 80;
    const UUID_V7_LAYOUT: u128 = 0x0000_0000_0000_7000_8000_0000_0000_0000;
    const FIRST: &str = "cargo check -p alpha --target left";
    const SECOND: &str = "cargo check -p beta --target right";
    const SECRET: &str = "do-not-print-this-private-input";
    const STREAM: &str = "private-subagent-label";
    const PACKAGE_SLOT: &str = "<value>";
    const TARGET_SLOT: &str = "<target>";
    const REFUSED_COMMAND: &str = "cargo check -p gamma --target middle";
    const REFUSAL: &str = "permission denied";
    const RESULT_SERIAL_OFFSET: u16 = 50;
    const TUPLE_MODE: &str = "observed tuples only";
    const STATE_KEY: &str = "permission.rules";
    const SAMPLING_CHECKS: u32 = 32;
    const FIRST_TAIL_ID: u16 = 100;
    const TAIL_ROWS: u16 = 64;
    const OTHER_FIRST: &str = "cargo test -p alpha --target left";
    const OTHER_SECOND: &str = "cargo test -p beta --target right";
    const LARGE_SESSION_ROWS: usize = MAX_ROWS_PER_SESSION * 4;
    const NOT_FULL_CALL_AUTHORIZATION: &str = "not full-call authorization";
    const HISTORY_TIMEOUT_SECS: u64 = 600;
    const HISTORY_TIMEOUT_MS: u64 = 600_000;
    const WORDED_DEADLINE: &str = "10m";

    #[derive(Clone, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Message(Value);

    impl TitleSource for Message {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    fn id(serial: u16, timestamp_ms: u64) -> CaudraId {
        CaudraId::from_bytes(
            ((u128::from(timestamp_ms) << UUID_TIME_SHIFT) | UUID_V7_LAYOUT | u128::from(serial))
                .to_be_bytes(),
        )
    }

    fn call(serial: u16, name: &str, input: Value) -> Value {
        json!({ "id": id(serial, TIME_MS), "group_id": id(serial, TIME_MS), "type": "tool_call", "call_id": format!("call-{serial}"), "name": name, "input": input })
    }

    fn shell(serial: u16, command: &str) -> Value {
        call(serial, "shell", json!({"command": command}))
    }

    fn result(serial: u16, refused: &[usize]) -> Value {
        let id = id(serial + RESULT_SERIAL_OFFSET, TIME_MS);
        let mut record = json!({ "id": id, "group_id": id, "type": "tool_result", "call_id": format!("call-{serial}"), "content": REFUSAL, "is_error": true });
        if !refused.is_empty() {
            record["refused_calls"] = json!(refused);
        }
        record
    }

    fn refused_calls_skipped(report: &DiscoveryReport) -> usize {
        report
            .processing
            .exclusions
            .get(&DiscoveryExclusion::RefusedCall)
            .copied()
            .unwrap_or_default()
    }

    fn fixture() -> (TempDir, StateDir, SessionDatabase) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let database = SessionDatabase::open(&state).unwrap();
        database.global_state_set(STATE_KEY, &json!([])).unwrap();
        (temp, state, database)
    }

    fn save(
        database: &mut SessionDatabase,
        serial: u16,
        project: &str,
        main: Vec<Value>,
        sub: Vec<Value>,
    ) {
        let mut session = Session::<Message, Value, Value>::new(MODEL, project);
        session.id = id(serial, TIME_MS);
        session.replace_messages(main.into_iter().map(Message).collect());
        if !sub.is_empty() {
            session.set_subagent_messages(STREAM.into(), sub.into_iter().map(Message).collect());
        }
        session.updated_at = u64::from(serial);
        database.save(&session, None).unwrap();
    }

    fn limits() -> DiscoveryLimits {
        DiscoveryLimits {
            max_elapsed_ms: MAX_ELAPSED_MS,
            ..DiscoveryLimits::default()
        }
    }

    fn assert_complete_imported_evidence(report: &DiscoveryReport) {
        assert_eq!(report.provenance.len(), report.candidates.len());
        for candidate in &report.candidates {
            let evidence = &candidate.evidence;
            let history = &report.provenance[&candidate.definition.fingerprint().unwrap()];
            assert_eq!(history.len(), evidence.support.observations);
            assert_eq!(evidence.provenance, ObservationProvenance::Imported);
            assert_eq!(
                evidence.outcomes[&InvocationOutcome::Unknown],
                history.len()
            );
            assert_eq!(
                history
                    .iter()
                    .map(|row| &row.observation_id)
                    .collect::<BTreeSet<_>>()
                    .len(),
                history.len()
            );
            assert_eq!(
                history
                    .iter()
                    .map(|row| row.session_id.to_string())
                    .collect::<BTreeSet<_>>()
                    .len(),
                evidence.support.independent_sessions
            );
        }
    }

    #[test_case("text"; "large_newest_session_cannot_starve_older_recurrence")]
    #[test_case("since"; "since_exclusions_consume_per_session_budget")]
    #[test_case("invalid"; "invalid_history_consumes_per_session_budget")]
    #[test_case("duplicate"; "duplicate_history_consumes_per_session_budget")]
    #[test_case("quarantine"; "cross_session_collisions_remove_capped_session_evidence")]
    fn fair_discovery_reaches_two_older_parents(kind: &str) {
        let (_temp, state, mut database) = fixture();
        let mut oldest = vec![shell(14, FIRST)];
        if kind == "quarantine" {
            oldest.push(shell(11, SECOND));
        }
        save(&mut database, 1, PROJECT, oldest, vec![]);
        save(&mut database, 2, PROJECT, vec![], vec![shell(12, SECOND)]);
        let newest = (0..LARGE_SESSION_ROWS)
            .map(|index| {
                let serial = FIRST_TAIL_ID + u16::try_from(index).unwrap();
                match kind {
                    "since" => {
                        let mut row = shell(serial, FIRST);
                        row["id"] = json!(id(serial, TIME_MS - 1));
                        row
                    }
                    "invalid" => json!({"id": "bad", "type": "tool_call"}),
                    "duplicate" => shell(FIRST_TAIL_ID, OTHER_FIRST),
                    "quarantine" => shell(11, FIRST),
                    _ => json!({"id": id(serial, TIME_MS), "type": "assistant_text"}),
                }
            })
            .collect();
        save(&mut database, 3, PROJECT, newest, vec![]);
        let before = database.raw_permission_snapshot().unwrap();
        drop(database);
        let bytes = fs::read(state.path().join(SESSIONS_DB_FILE)).unwrap();
        let mut limits = limits();
        limits.history.max_rows = MAX_ROWS_PER_SESSION + 3;
        limits.since_ms = Some(TIME_MS);
        let report =
            discover_with_budget(&state, Path::new(PROJECT), limits, || false, |_| false).unwrap();
        assert_eq!(report.sample.sessions, 3);
        assert_eq!(report.sample.session_row_cutoffs, 1);
        assert!(report.sample.truncated);
        assert!(!report.sample.stopped);
        assert_eq!(
            report.sample.max_rows_per_session,
            Some(MAX_ROWS_PER_SESSION)
        );
        assert_eq!(report.sample.per_session[0].session_id, id(3, TIME_MS));
        assert_eq!(report.sample.per_session[0].rows, MAX_ROWS_PER_SESSION);
        assert!(report.sample.per_session[0].row_cutoff);
        assert_eq!(
            report.sample.rows,
            MAX_ROWS_PER_SESSION + 2 + usize::from(kind == "quarantine")
        );
        match kind {
            "since" => assert_eq!(
                report.processing.exclusions[&DiscoveryExclusion::BeforeSince],
                MAX_ROWS_PER_SESSION
            ),
            "invalid" => assert_eq!(report.sample.invalid_records, MAX_ROWS_PER_SESSION),
            "duplicate" | "quarantine" => assert_eq!(
                report.processing.duplicate_records,
                MAX_ROWS_PER_SESSION - 1
            ),
            _ => assert_eq!(
                report.processing.exclusions[&DiscoveryExclusion::NonToolRecord],
                MAX_ROWS_PER_SESSION
            ),
        }
        assert_eq!(
            report.processing.colliding_records,
            usize::from(kind == "quarantine")
        );
        assert_eq!(report.candidates.len(), 1, "{report:?}");
        assert_complete_imported_evidence(&report);
        assert_eq!(report.candidates[0].evidence.support.observations, 2);
        assert_eq!(
            report.candidates[0].evidence.support.independent_sessions,
            2
        );
        let history = report.provenance.values().flatten().collect::<Vec<_>>();
        assert!(history.iter().all(|row| row.session_id != id(3, TIME_MS)));
        assert!(history.iter().any(|row| row.subagent_id_hash.is_some()));
        assert_eq!(
            fs::read(state.path().join(SESSIONS_DB_FILE)).unwrap(),
            bytes
        );
        assert_eq!(
            SessionDatabase::open_read_only(&state)
                .unwrap()
                .raw_permission_snapshot()
                .unwrap(),
            before
        );
    }

    #[test_case(0, 0; "zero_session_budget")]
    #[test_case(1, 1; "one_row_per_parent")]
    #[test_case(usize::MAX, MAX_ROWS_PER_SESSION; "per_session_budget_is_hard_bounded")]
    fn per_session_discovery_limit_is_explicit(requested: usize, effective: usize) {
        let (_temp, state, mut database) = fixture();
        save(&mut database, 1, PROJECT, vec![shell(11, FIRST)], vec![]);
        save(&mut database, 2, PROJECT, vec![shell(12, SECOND)], vec![]);
        let limits = DiscoveryLimits {
            max_rows_per_session: requested,
            ..DiscoveryLimits::default()
        }
        .bounded()
        .unwrap();
        let report =
            discover_with_budget(&state, Path::new(PROJECT), limits, || false, |_| false).unwrap();
        assert_eq!(report.limits.max_rows_per_session, effective);
        assert_eq!(report.sample.max_rows_per_session, Some(effective));
        assert_eq!(
            report.sample.session_row_cutoffs,
            if effective == 0 { 2 } else { 0 }
        );
        assert_eq!(report.sample.rows, if effective == 0 { 0 } else { 2 });
        assert_eq!(report.candidates.len(), usize::from(effective > 0));
    }

    #[test_case("cargo check -p alpha; python -c 'print(1)'", 2, 1, Some(PatternOmissionReason::InterpretedExecutable), PatternObligationKind::Operator(BashOperatorKind::Semicolon); "partial_call_observations_do_not_cover_payload_siblings")]
    #[test_case("cargo check -p alpha &", 1, 1, None, PatternObligationKind::Operator(BashOperatorKind::Background); "observed_commands_leave_source_obligations")]
    #[test_case("cargo check -p alpha >/dev/null", 1, 0, Some(PatternOmissionReason::ShellRedirects), PatternObligationKind::Redirect; "omitted_redirect_scope")]
    #[test_case("cargo check --token do-not-print-this-private-input", 1, 0, Some(PatternOmissionReason::SensitiveArguments), PatternObligationKind::ProgramEffectsNotAssessed; "sensitive_omissions_are_sanitized")]
    #[test_case("git -C /fixture status", 1, 0, Some(PatternOmissionReason::PotentialPayloadArgument), PatternObligationKind::ProgramEffectsNotAssessed; "potential_payload_omissions")]
    #[test_case("'cargo' check -p alpha", 1, 0, Some(PatternOmissionReason::ExecutableIdentity), PatternObligationKind::ProgramEffectsNotAssessed; "identity_omissions")]
    fn discovery_aggregates_typed_scope_diagnostics(
        source: &str,
        represented: usize,
        observed: usize,
        omission: Option<PatternOmissionReason>,
        obligation: PatternObligationKind,
    ) {
        let (_temp, state, mut database) = fixture();
        for serial in [1, 2] {
            save(
                &mut database,
                serial,
                PROJECT,
                vec![shell(serial, source)],
                vec![],
            );
        }
        let report =
            discover_with_budget(&state, Path::new(PROJECT), limits(), || false, |_| false)
                .unwrap();
        let stats = &report.processing;
        assert_eq!(stats.analyzed_shell_calls, 2);
        assert_eq!(stats.represented_commands, represented * 2);
        assert_eq!(stats.observed_commands, observed * 2);
        assert_eq!(
            stats.calls_with_omitted_commands,
            usize::from(omission.is_some()) * 2
        );
        if let Some(reason) = omission {
            assert_eq!(stats.omission_counts[&reason], 2);
        } else {
            assert!(stats.omission_counts.is_empty());
        }
        assert_eq!(
            stats
                .obligation_counts
                .iter()
                .find(|count| count.kind == obligation)
                .unwrap()
                .count,
            2
        );
        assert_eq!(stats.analysis_failures, 0);
        let exported = serde_json::to_string(stats).unwrap();
        assert!(!exported.contains(SECRET));
        assert!(!exported.contains(PROJECT));
        assert!(!exported.contains("span"));
        assert!(render_human(&report).contains(NOT_FULL_CALL_AUTHORIZATION));
        assert_eq!(report.candidates.len(), usize::from(observed > 0));
        assert_complete_imported_evidence(&report);
    }

    #[test_case("python3 - <<'PY'\nprint(1)\nPY\n"; "incomplete_source_is_counted")]
    #[test_case("printf -v CDPATH /elsewhere; cargo check"; "incomplete_context_is_counted")]
    #[test_case("cd left || cd right; cat note.txt"; "known_cwd_alternatives_remain_omitted")]
    fn discovery_keeps_incomplete_scope_counts(source: &str) {
        let (_temp, state, mut database) = fixture();
        for serial in [1, 2] {
            save(
                &mut database,
                serial,
                PROJECT,
                vec![shell(serial, source)],
                vec![],
            );
        }
        let analysis = analyze_pattern_calls(
            source,
            Path::new(PROJECT),
            Path::new(PROJECT),
            standard_bash_assumptions(),
        )
        .unwrap();
        let report =
            discover_with_budget(&state, Path::new(PROJECT), limits(), || false, |_| false)
                .unwrap();
        assert_eq!(
            report.processing.calls_with_incomplete_source,
            usize::from(!analysis.diagnostics.obligations.source_coverage_complete) * 2
        );
        assert_eq!(
            report.processing.calls_with_incomplete_context,
            usize::from(!analysis.diagnostics.obligations.context_complete) * 2
        );
        assert!(
            report.processing.calls_with_incomplete_source
                + report.processing.calls_with_incomplete_context
                + report.processing.calls_with_omitted_commands
                > 0
        );
        assert!(report.candidates.is_empty());
    }

    #[test_case(true, 2; "previously_added_then_quarantined_observation_is_not_provenance")]
    #[test_case(false, 2; "matching_but_capacity_refused_observation_is_not_provenance")]
    fn provenance_uses_only_retained_recognizer_evidence(quarantine: bool, cap: usize) {
        let (_temp, state, _database) = fixture();
        let mut report =
            discover_with_budget(&state, Path::new(PROJECT), limits(), || false, |_| false)
                .unwrap();
        report.recognizer_limits = RecognizerLimits {
            max_observations: cap + 1,
            max_clusters: 1,
            max_cluster_observations: cap + usize::from(quarantine),
            ..RecognizerLimits::default()
        };
        let rows: Vec<_> = (0..=cap)
            .map(|index| {
                let serial = u16::try_from(index).unwrap();
                let history_id = id(serial, TIME_MS);
                let mut observation = analyze_pattern_calls(
                    FIRST,
                    Path::new(PROJECT),
                    Path::new(PROJECT),
                    standard_bash_assumptions(),
                )
                .unwrap()
                .observations
                .remove(0);
                observation.source.source_identity = report.source_identity.clone();
                observation.source.observation_id = format!("{history_id}:0:0");
                observation.source.session_id = history_id.to_string();
                observation.source.timestamp_ms = TIME_MS;
                AnalyzedObservation {
                    history: HistoryEvidence {
                        session_id: history_id,
                        history_id,
                        timestamp_ms: TIME_MS,
                        ordinal: 0,
                        subagent_id_hash: None,
                        observation_id: observation.source.observation_id.clone(),
                    },
                    observation,
                }
            })
            .collect();
        let excluded_index = if quarantine { 0 } else { cap };
        let excluded_id = rows[excluded_index].history.history_id;
        let excluded_observation = rows[excluded_index].observation.clone();
        let mut recognizer =
            PatternRecognizer::new(report.recognizer_limits.clone(), report.as_of_ms).unwrap();
        let mut accepted = Vec::new();
        for row in rows {
            let outcome = recognizer.observe(row.observation.clone());
            if !quarantine && row.history.history_id == excluded_id {
                assert!(matches!(outcome, Err(RecognitionError::Capacity(_))));
            } else {
                assert_eq!(outcome.unwrap(), ObserveOutcome::Added);
                accepted.push(row);
            }
        }
        if quarantine {
            assert!(recognizer.retains(&excluded_observation));
            let mut collision = excluded_observation.clone();
            collision.source.timestamp_ms += 1;
            assert!(matches!(
                recognizer.observe(collision),
                Err(RecognitionError::Collision)
            ));
        }
        assert!(!recognizer.retains(&excluded_observation));
        report_candidates(&mut report, &recognizer, accepted, &|_| false);
        assert_eq!(report.recognition.quarantined_ids, usize::from(quarantine));
        assert_eq!(
            report.recognition.accepted,
            u64::try_from(cap + usize::from(quarantine)).unwrap()
        );
        assert_eq!(report.candidates.len(), 1);
        assert_complete_imported_evidence(&report);
        assert_eq!(report.candidates[0].evidence.support.observations, cap);
        assert!(
            CompiledPattern::compile(&report.candidates[0].definition)
                .unwrap()
                .matches(&excluded_observation)
                .unwrap()
                .is_match()
        );
        assert!(
            report
                .provenance
                .values()
                .flatten()
                .all(|row| row.history_id != excluded_id)
        );
        let exclusion = if quarantine {
            RecognitionExclusion::Collision
        } else {
            RecognitionExclusion::Capacity
        };
        assert_eq!(report.recognition.exclusions[&exclusion], 1);
        let human = render_human(&report);
        assert!(human.contains(&format!(
            "{RECOGNIZER_CAPACITY}: {}",
            usize::from(!quarantine)
        )));
        assert!(human.contains(RECOGNIZER_ORDER_BIAS));
    }

    #[test_case("nested"; "batch_nested_parameters")]
    #[test_case("flat"; "batch_flat_parameters")]
    #[test_case("mixed"; "batch_mixed_nonconflicting_parameters")]
    fn native_and_batch_discovery_reports_correlated_imported_evidence_without_writes(shape: &str) {
        let (_temp, state, mut database) = fixture();
        let child = match shape {
            "flat" => json!({"tool": "functions.shell", "command": SECOND}),
            "mixed" => json!({"tool": "shell", "command": SECOND, "parameters": {"workdir": "."}}),
            _ => json!({"tool": "shell", "parameters": {"command": SECOND}}),
        };
        save(&mut database, 1, PROJECT, vec![shell(11, FIRST)], vec![]);
        save(
            &mut database,
            2,
            PROJECT,
            vec![],
            vec![call(12, "batch", json!({"tool_calls": [child]}))],
        );
        save(
            &mut database,
            3,
            OTHER_PROJECT,
            vec![shell(13, FIRST)],
            vec![],
        );
        let before = database.raw_permission_snapshot().unwrap();
        drop(database);
        let database_bytes = fs::read(state.path().join(SESSIONS_DB_FILE)).unwrap();
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(report.sample.sessions, 2);
        assert_eq!(report.candidates.len(), 1, "{report:?}");
        assert!(!report.historical_context_verified);
        let candidate = &report.candidates[0];
        assert_eq!(
            candidate.evidence.provenance,
            ObservationProvenance::Imported
        );
        assert_eq!(candidate.evidence.support.observations, 2);
        assert_eq!(candidate.evidence.support.independent_sessions, 2);
        assert_eq!(candidate.evidence.outcomes[&InvocationOutcome::Unknown], 2);
        assert_eq!(candidate.evidence.first_seen_ms, TIME_MS);
        assert_eq!(candidate.evidence.last_seen_ms, TIME_MS);
        assert_eq!(candidate.definition.slots.len(), 2);
        assert!(
            matches!(&candidate.definition.combinations, SlotCombinations::ObservedTuples { tuples } if tuples.len() == 2)
        );
        assert!(
            candidate
                .definition
                .slots
                .iter()
                .all(|slot| slot.option_like == OptionLikePolicy::Reject)
        );
        let evidence = &report.provenance[&candidate.definition.fingerprint().unwrap()];
        assert_eq!(evidence.len(), 2);
        assert_eq!(
            evidence
                .iter()
                .map(|row| row.session_id.to_string())
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );
        assert!(evidence.iter().any(|row| row.subagent_id_hash.is_some()));
        let exported = serde_json::to_string(&report).unwrap();
        assert!(exported.contains("alpha"));
        assert!(exported.contains("beta"));
        assert!(!exported.contains(STREAM));
        let human = render_human(&report);
        assert!(human.contains(PACKAGE_SLOT));
        assert!(human.contains(TARGET_SLOT));
        assert!(human.contains(TUPLE_MODE));
        assert!(!human.contains(FIRST));
        assert_eq!(
            fs::read(state.path().join(SESSIONS_DB_FILE)).unwrap(),
            database_bytes
        );
        assert_eq!(
            SessionDatabase::open_read_only(&state)
                .unwrap()
                .raw_permission_snapshot()
                .unwrap(),
            before
        );

        let mut renamed = candidate.definition.clone();
        renamed.name = "user renamed this".into();
        renamed.slots[0].label = "user label".into();
        let compiled = CompiledPattern::compile(&renamed).unwrap();
        assert_eq!(
            compiled.fingerprint(),
            candidate.definition.fingerprint().unwrap()
        );
        for (command, expected) in [
            (FIRST, true),
            (SECOND, true),
            ("cargo check -p alpha --target right", false),
            ("cargo check -p other --target left", false),
        ] {
            let analysis = analyze_pattern_calls(
                command,
                Path::new(PROJECT),
                Path::new(PROJECT),
                standard_bash_assumptions(),
            )
            .unwrap();
            assert_eq!(
                compiled
                    .matches(&analysis.observations[0])
                    .unwrap()
                    .is_match(),
                expected
            );
        }
    }

    #[test_case(&[0], 1, 0; "a_refused_call_is_skipped")]
    #[test_case(&[], 0, 1; "a_result_saved_before_refusals_were_recorded_counts_its_call")]
    fn refused_calls_teach_nothing(refused: &[usize], skipped: usize, candidates: usize) {
        let (_temp, state, mut database) = fixture();
        save(
            &mut database,
            1,
            PROJECT,
            vec![shell(11, FIRST), result(11, &[])],
            vec![],
        );
        save(
            &mut database,
            2,
            PROJECT,
            vec![shell(12, SECOND), result(12, refused)],
            vec![],
        );
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(refused_calls_skipped(&report), skipped);
        assert_eq!(report.candidates.len(), candidates, "{report:?}");
        assert!(render_human(&report).contains(&format!("{REFUSED_CALLS_SKIPPED}: {skipped}.")));
        assert_eq!(
            serde_json::to_value(&report).unwrap()["processing"]["exclusions"]["refused_call"]
                .as_u64(),
            (skipped > 0).then_some(1)
        );
    }

    #[test_case(false; "main_history")]
    #[test_case(true; "subagent_history")]
    fn a_refused_batch_entry_is_skipped_while_its_siblings_count(subagent: bool) {
        let (_temp, state, mut database) = fixture();
        save(&mut database, 1, PROJECT, vec![shell(11, FIRST)], vec![]);
        let batch = call(
            12,
            "batch",
            json!({"tool_calls": [
                {"tool": "shell", "command": REFUSED_COMMAND},
                {"tool": "shell", "command": SECOND},
            ]}),
        );
        let records = vec![batch, result(12, &[0])];
        let (main, sub) = if subagent {
            (vec![], records)
        } else {
            (records, vec![])
        };
        save(&mut database, 2, PROJECT, main, sub);
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(refused_calls_skipped(&report), 1);
        assert_eq!(report.candidates.len(), 1, "{report:?}");
        assert_eq!(report.candidates[0].evidence.support.observations, 2);
    }

    #[test_case(true; "another_session")]
    #[test_case(false; "another_stream_of_the_session")]
    fn a_refusal_names_only_the_call_in_its_own_stream(other_session: bool) {
        let (_temp, state, mut database) = fixture();
        if other_session {
            save(&mut database, 1, PROJECT, vec![shell(11, FIRST)], vec![]);
            save(
                &mut database,
                2,
                PROJECT,
                vec![result(11, &[0]), shell(12, SECOND)],
                vec![],
            );
        } else {
            save(
                &mut database,
                1,
                PROJECT,
                vec![result(11, &[0])],
                vec![shell(11, FIRST)],
            );
            save(&mut database, 2, PROJECT, vec![shell(12, SECOND)], vec![]);
        }
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(refused_calls_skipped(&report), 0);
        assert_eq!(report.candidates.len(), 1, "{report:?}");
    }

    #[test_case(false; "copied_history_does_not_create_independent_support")]
    #[test_case(true; "subagents_are_not_independent_parent_sessions")]
    fn independent_sessions_are_not_inflated(subagents: bool) {
        let (_temp, state, mut database) = fixture();
        save(
            &mut database,
            1,
            PROJECT,
            vec![shell(11, FIRST)],
            if subagents {
                vec![shell(12, SECOND)]
            } else {
                vec![]
            },
        );
        if !subagents {
            save(&mut database, 2, PROJECT, vec![shell(11, FIRST)], vec![]);
        }
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.processing.duplicate_records, usize::from(!subagents));
    }

    #[test_case("shell"; "conflicting_command")]
    #[test_case("mcp__server__shell"; "conflicting_unsupported_call_also_quarantines")]
    fn collisions_remove_all_prior_observations_from_that_history_id(name: &str) {
        let (_temp, state, mut database) = fixture();
        save(
            &mut database,
            1,
            PROJECT,
            vec![shell(11, FIRST), shell(14, SECOND)],
            vec![],
        );
        let mut conflict = shell(11, SECOND);
        conflict["name"] = json!(name);
        save(&mut database, 2, PROJECT, vec![conflict], vec![]);
        save(&mut database, 3, PROJECT, vec![shell(11, FIRST)], vec![]);
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(report.processing.colliding_records, 1);
        assert_eq!(report.processing.quarantined_records, 1);
        assert_eq!(report.recognition.retained_observations, 1);
        assert!(report.candidates.is_empty());
    }

    #[test_case("shell", "cargo check -p alpha --token do-not-print-this-private-input"; "sensitive_argument")]
    #[test_case("shell", "bash -c 'echo do-not-print-this-private-input'"; "shell_payload")]
    #[test_case("shell", "python -c 'print(1)'"; "interpreter_payload")]
    #[test_case("shell", "cargo check -p $(echo alpha)"; "dynamic_argument")]
    #[test_case("mcp__server__shell", FIRST; "mcp_is_not_native")]
    #[test_case("server.shell", FIRST; "unknown_namespaced_tool")]
    #[test_case("bash", FIRST; "legacy_name_is_not_inferred_native")]
    #[test_case("functions.functions.shell", FIRST; "multiple_prefixes_are_not_stripped")]
    fn unsupported_calls_never_leak_inputs(name: &str, command: &str) {
        let (_temp, state, mut database) = fixture();
        for serial in [1, 2] {
            let mut record = call(serial, name, json!({"command": command}));
            record["call_id"] = json!(SECRET);
            save(&mut database, serial, PROJECT, vec![record], vec![]);
        }
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.recognition.retained_observations, 0);
        assert!(!serde_json::to_string(&report).unwrap().contains(SECRET));
        assert!(!render_human(&report).contains(SECRET));
    }

    #[test_case("user"; "user_json_is_not_a_call")]
    #[test_case("tool_result"; "output_json_is_not_a_call")]
    #[test_case("assistant_text"; "assistant_text_is_not_a_call")]
    fn tool_shaped_text_and_outputs_are_not_recursively_mined(kind: &str) {
        let (_temp, state, mut database) = fixture();
        let mut record = shell(11, FIRST);
        record["type"] = json!(kind);
        record["content"] = json!([shell(12, SECOND)]);
        save(&mut database, 1, PROJECT, vec![record], vec![]);
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(report.processing.calls, 0);
        assert_eq!(
            report.processing.exclusions[&DiscoveryExclusion::NonToolRecord],
            1
        );
    }

    #[test_case("duplicate_parameter"; "batch_rejects_conflicting_fields")]
    #[test_case("string_input"; "string_encoded_inputs_are_not_guessed")]
    #[test_case("extra_input"; "unrecognized_input_fields_are_not_exported")]
    #[test_case("nested_batch"; "nested_batches_are_not_recursed")]
    #[test_case("parent_workdir"; "parent_components_are_not_fs_resolved")]
    #[test_case("worded_deadline"; "a_deadline_that_is_not_a_count_is_not_guessed")]
    #[test_case("large_command"; "oversized_command_not_analyzed")]
    fn malformed_or_out_of_scope_inputs_are_omitted(case: &str) {
        let (_temp, state, mut database) = fixture();
        let record = match case {
            "duplicate_parameter" => call(
                11,
                "batch",
                json!({"tool_calls": [{"tool": "shell", "command": FIRST, "parameters": {"command": SECOND}}]}),
            ),
            "string_input" => call(11, "shell", json!(json!({"command": FIRST}).to_string())),
            "extra_input" => call(11, "shell", json!({"command": FIRST, "token": SECRET})),
            "nested_batch" => call(
                11,
                "batch",
                json!({"tool_calls": [{"tool": "batch", "parameters": {"tool_calls": [{"tool": "shell", "command": FIRST}]}}]}),
            ),
            "parent_workdir" => call(
                11,
                "shell",
                json!({"command": FIRST, "workdir": "../elsewhere"}),
            ),
            "worded_deadline" => call(
                11,
                "shell",
                json!({"command": FIRST, "timeoutSec": WORDED_DEADLINE}),
            ),
            _ => shell(11, &"x".repeat(MAX_COMMAND_BYTES + 1)),
        };
        save(&mut database, 1, PROJECT, vec![record], vec![]);
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(report.recognition.retained_observations, 0);
        assert!(!serde_json::to_string(&report).unwrap().contains(SECRET));
    }

    /// A deadline is not what makes a command worth a rule, and older sessions
    /// still spell it in milliseconds under the key it had then.
    #[test_case(json!({"command": FIRST, "timeoutSec": HISTORY_TIMEOUT_SECS}); "in_seconds")]
    #[test_case(json!({"command": FIRST, "timeout": HISTORY_TIMEOUT_MS}); "in_milliseconds_from_older_sessions")]
    fn a_recorded_deadline_does_not_hide_the_call(input: Value) {
        let (_temp, state, mut database) = fixture();
        save(
            &mut database,
            1,
            PROJECT,
            vec![call(11, "shell", input)],
            vec![],
        );
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(report.processing.analyzed_shell_calls, 1);
    }

    #[test_case(DATE, TIME_MS; "inclusive_millisecond")]
    #[test_case("2026-01-01T00:00:00.000000001Z", TIME_MS + 1; "fractional_cutoff_rounds_up")]
    fn since_uses_history_creation_time_not_session_activity(value: &str, cutoff: u64) {
        let (_temp, state, mut database) = fixture();
        for serial in [1, 2] {
            save(
                &mut database,
                serial,
                PROJECT,
                vec![shell(serial, FIRST)],
                vec![],
            );
        }
        assert_eq!(parse_since(value).unwrap(), cutoff);
        let report = discover_for_project(
            &state,
            Path::new(PROJECT),
            DiscoveryLimits {
                since_ms: Some(cutoff),
                ..limits()
            },
        )
        .unwrap();
        assert_eq!(report.candidates.len(), usize::from(cutoff == TIME_MS));
        assert_eq!(report.sample.rows, 2);
        assert!(report.sample.bytes > 0);
    }

    #[test_case("not-a-date"; "invalid_date")]
    #[test_case("1969-12-31T23:59:59.999999999Z"; "negative_submillisecond")]
    fn invalid_since_is_redacted(value: &str) {
        assert_eq!(parse_since(value).unwrap_err().to_string(), INVALID_SINCE);
    }

    #[test_case("bad"; "invalid_base58")]
    #[test_case("019b76da-a800-7000-8000-000000000001"; "hex_is_not_storage_id")]
    #[test_case("1111111111111111"; "nil_is_not_v7")]
    fn invalid_ids_do_not_get_session_timestamp_fallback(value: &str) {
        let (_temp, state, mut database) = fixture();
        let mut record = shell(11, FIRST);
        record["id"] = json!(value);
        save(&mut database, 1, PROJECT, vec![record], vec![]);
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(report.sample.invalid_records, 1);
        assert_eq!(report.processing.calls, 0);
    }

    #[test_case(MAX_TIMESTAMP_MS; "future_history_is_not_relabelled_with_current_time")]
    fn future_history_is_excluded(timestamp_ms: u64) {
        let (_temp, state, mut database) = fixture();
        let mut record = shell(11, FIRST);
        record["id"] = json!(id(11, timestamp_ms));
        save(&mut database, 1, PROJECT, vec![record], vec![]);
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert_eq!(
            report.processing.exclusions[&DiscoveryExclusion::FutureTimestamp],
            1
        );
        assert_eq!(report.processing.calls, 0);
    }

    #[test_case("rows"; "duplicates_consume_row_budget")]
    #[test_case("bytes"; "duplicates_consume_byte_budget")]
    #[test_case("row_bytes"; "oversized_rows_count_without_parsing")]
    #[test_case("calls"; "calls_in_batch_consume_global_budget")]
    #[test_case("analysis"; "source_analysis_has_total_byte_budget")]
    fn sampling_and_analysis_are_bounded(case: &str) {
        let (_temp, state, mut database) = fixture();
        let record = if case == "calls" {
            call(
                11,
                "batch",
                json!({"tool_calls": [{"tool": "shell", "command": FIRST}, {"tool": "shell", "command": SECOND}]}),
            )
        } else {
            shell(11, FIRST)
        };
        let row_bytes = serde_json::to_vec(&record).unwrap().len();
        save(&mut database, 1, PROJECT, vec![record; 4], vec![]);
        let mut limits = limits();
        match case {
            "rows" => limits.history.max_rows = 2,
            "bytes" => limits.history.max_bytes = row_bytes * 2,
            "row_bytes" => limits.history.max_row_bytes = 1,
            "calls" => limits.max_calls = 1,
            _ => limits.max_analysis_bytes = FIRST.len() - 1,
        }
        let report = discover_for_project(&state, Path::new(PROJECT), limits).unwrap();
        match case {
            "rows" | "bytes" => {
                assert_eq!(report.sample.rows, 2);
                assert_eq!(report.sample.bytes, row_bytes * 2);
                assert_eq!(report.processing.duplicate_records, 1);
                assert!(report.sample.truncated);
            }
            "row_bytes" => {
                assert_eq!(report.sample.oversized_rows, 4);
                assert_eq!(report.processing.calls, 0);
            }
            "calls" => {
                assert!(report.processing.call_limit);
                assert_eq!(report.processing.calls, 1);
            }
            _ => {
                assert!(report.processing.analysis_byte_limit);
                assert_eq!(report.processing.analysis_bytes, 0);
            }
        }
    }

    #[test_case(true; "partial_sample_preserves_deduplication_and_quarantine")]
    #[test_case(false; "single_session_partial_sample_does_not_inflate_support")]
    fn sampling_deadline_reserves_recognition_and_evidence(independent_sessions: bool) {
        let (_temp, state, mut database) = fixture();
        let mut older: Vec<_> = (FIRST_TAIL_ID..FIRST_TAIL_ID + TAIL_ROWS)
            .map(|serial| json!({"id": id(serial, TIME_MS), "type": "assistant_text"}))
            .collect();
        older.extend([
            shell(13, FIRST),
            call(13, "mcp__server__shell", json!({"command": SECOND})),
            shell(12, SECOND),
            shell(11, FIRST),
        ]);
        let newer = vec![shell(13, FIRST), shell(12, SECOND)];
        let total_rows = older.len() + newer.len();
        if independent_sessions {
            save(&mut database, 1, PROJECT, older, vec![]);
            save(&mut database, 2, PROJECT, newer, vec![]);
        } else {
            older.extend(newer);
            save(&mut database, 1, PROJECT, older, vec![]);
        }
        let before = database.raw_permission_snapshot().unwrap();
        drop(database);
        let database_bytes = fs::read(state.path().join(SESSIONS_DB_FILE)).unwrap();
        let limits = DiscoveryLimits::default();
        let budget = Duration::from_millis(limits.max_elapsed_ms);
        let tick = budget / SAMPLING_BUDGET_DIVISOR / SAMPLING_CHECKS;
        let elapsed = Cell::new(Duration::ZERO);
        let report = discover_with_budget(
            &state,
            Path::new(PROJECT),
            limits,
            || false,
            |phase| {
                if matches!(phase, DiscoveryPhase::Sampling) {
                    elapsed.set(elapsed.get() + tick);
                }
                elapsed.get() >= phase.deadline(budget)
            },
        )
        .unwrap();
        assert_eq!(elapsed.get(), budget / SAMPLING_BUDGET_DIVISOR);
        assert!(report.sample.stopped);
        assert!(report.sample.rows < total_rows);
        assert!(report.processing.sampling_time_limit);
        assert!(!report.processing.recognition_time_limit);
        assert!(!report.processing.timed_out);
        assert!(!report.processing.cancelled);
        assert!(!report.processing.recognition_failed);
        assert_eq!(report.processing.duplicate_records, 1);
        assert_eq!(report.processing.colliding_records, 1);
        assert_eq!(report.processing.quarantined_records, 1);
        assert_eq!(report.recognition.received, 2);
        assert_eq!(report.recognition.accepted, 2);
        assert_eq!(report.recognition.retained_observations, 2);
        assert_eq!(report.candidates.len(), usize::from(independent_sessions));
        assert_complete_imported_evidence(&report);
        if independent_sessions {
            assert_eq!(report.candidates[0].evidence.support.observations, 2);
            assert_eq!(
                report.candidates[0].evidence.support.independent_sessions,
                2
            );
            assert_eq!(
                report
                    .provenance
                    .values()
                    .flatten()
                    .map(|row| row.history_id.to_string())
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([id(11, TIME_MS).to_string(), id(12, TIME_MS).to_string()])
            );
        }
        assert_eq!(
            fs::read(state.path().join(SESSIONS_DB_FILE)).unwrap(),
            database_bytes
        );
        assert_eq!(
            SessionDatabase::open_read_only(&state)
                .unwrap()
                .raw_permission_snapshot()
                .unwrap(),
            before
        );
    }

    #[test_case(3, 1; "admission_cutoff_cannot_fabricate_support")]
    #[test_case(5, 2; "admission_cutoff_preserves_reporting_budget")]
    fn recognition_deadline_reports_only_admitted_support(checks: u32, retained: usize) {
        let (_temp, state, mut database) = fixture();
        save(
            &mut database,
            1,
            PROJECT,
            vec![shell(11, FIRST), shell(13, FIRST)],
            vec![],
        );
        save(
            &mut database,
            2,
            PROJECT,
            vec![shell(12, SECOND), shell(14, SECOND)],
            vec![],
        );
        let limits = DiscoveryLimits::default();
        let budget = Duration::from_millis(limits.max_elapsed_ms);
        let deadline = DiscoveryPhase::Recognition.deadline(budget);
        let tick = deadline / checks;
        let elapsed = Cell::new(Duration::ZERO);
        let report = discover_with_budget(
            &state,
            Path::new(PROJECT),
            limits,
            || false,
            |phase| {
                if matches!(phase, DiscoveryPhase::Recognition) {
                    elapsed.set(elapsed.get() + tick);
                }
                elapsed.get() >= phase.deadline(budget)
            },
        )
        .unwrap();
        assert_eq!(elapsed.get(), deadline);
        assert_eq!(report.sample.rows, 4);
        assert!(!report.sample.stopped);
        assert!(!report.processing.sampling_time_limit);
        assert!(report.processing.recognition_time_limit);
        assert!(!report.processing.timed_out);
        assert!(!report.processing.cancelled);
        assert!(!report.processing.recognition_failed);
        assert_eq!(
            report.recognition.received,
            u64::try_from(retained).unwrap()
        );
        assert_eq!(report.recognition.retained_observations, retained);
        assert_eq!(report.candidates.len(), usize::from(retained == 2));
        assert_complete_imported_evidence(&report);
        if let Some(candidate) = report.candidates.first() {
            assert_eq!(candidate.evidence.support.observations, retained);
        }
    }

    #[test_case(false, 8, 0; "deadline_discards_incomplete_candidate_evidence")]
    #[test_case(false, 16, 1; "deadline_retains_completed_candidates")]
    #[test_case(true, 8, 0; "cancellation_during_candidate_evidence")]
    #[test_case(true, 16, 0; "cancellation_discards_completed_candidates")]
    fn reporting_deadline_and_cancellation_preserve_complete_evidence(
        cancel: bool,
        checks: u32,
        candidates: usize,
    ) {
        let (_temp, state, mut database) = fixture();
        save(
            &mut database,
            1,
            PROJECT,
            vec![shell(11, FIRST), shell(13, OTHER_FIRST)],
            vec![],
        );
        save(
            &mut database,
            2,
            PROJECT,
            vec![shell(12, SECOND), shell(14, OTHER_SECOND)],
            vec![],
        );
        let limits = DiscoveryLimits::default();
        let budget = Duration::from_millis(limits.max_elapsed_ms);
        let tick = budget / checks;
        let elapsed = Cell::new(Duration::ZERO);
        let cancellation = Cell::new(false);
        let report = discover_with_budget(
            &state,
            Path::new(PROJECT),
            limits,
            || cancellation.replace(false),
            |phase| {
                if matches!(phase, DiscoveryPhase::Reporting) {
                    elapsed.set(elapsed.get() + tick);
                    if cancel && elapsed.get() >= budget {
                        cancellation.set(true);
                        return false;
                    }
                }
                elapsed.get() >= phase.deadline(budget)
            },
        )
        .unwrap();
        assert_eq!(elapsed.get(), budget);
        assert_eq!(report.recognition.retained_observations, 4);
        assert!(!report.processing.sampling_time_limit);
        assert!(!report.processing.recognition_time_limit);
        assert!(!report.processing.recognition_failed);
        assert_eq!(report.processing.timed_out, !cancel);
        assert_eq!(report.processing.cancelled, cancel);
        assert!(!cancellation.get());
        assert_eq!(report.candidates.len(), candidates);
        assert_complete_imported_evidence(&report);
    }

    #[test_case(false; "cancellation_before_open")]
    #[test_case(true; "zero_deadline_before_open")]
    fn cancelled_or_expired_jobs_do_not_open_or_create_storage(expired: bool) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("not-created"));
        let report = discover_for_project_cancellable(
            &state,
            Path::new(PROJECT),
            DiscoveryLimits {
                max_elapsed_ms: if expired { 0 } else { MAX_ELAPSED_MS },
                ..limits()
            },
            || !expired,
        )
        .unwrap();
        assert!(report.processing.cancelled || report.processing.timed_out);
        assert!(report.candidates.is_empty());
        assert!(!state.path().exists());
    }

    #[test_case(2; "cancellation_remains_latched")]
    fn mid_scan_cancellation_is_sticky(cancel_after: usize) {
        let (_temp, state, mut database) = fixture();
        save(&mut database, 1, PROJECT, vec![shell(11, FIRST)], vec![]);
        let checks = Cell::new(0);
        let report = discover_for_project_cancellable(&state, Path::new(PROJECT), limits(), || {
            checks.set(checks.get() + 1);
            checks.get() == cancel_after
        })
        .unwrap();
        assert!(report.processing.cancelled);
        assert!(report.candidates.is_empty());
    }

    #[test_case(0, 1; "zero_suggestions_clamped")]
    #[test_case(usize::MAX, MAX_RECOGNIZER_SUGGESTIONS; "suggestions_hard_cap")]
    fn missing_database_reports_unavailable_without_creating_files(
        requested: usize,
        effective: usize,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("not-created"));
        let report = discover_for_project(
            &state,
            Path::new(PROJECT),
            DiscoveryLimits {
                max_suggestions: requested,
                ..limits()
            },
        )
        .unwrap();
        assert!(report.processing.storage_unavailable);
        assert!(report.candidates.is_empty());
        assert_eq!(report.limits.max_suggestions, effective);
        assert!(!state.path().exists());
    }

    #[test_case("relative"; "relative_project_refused")]
    #[test_case("/history/../elsewhere"; "parent_project_refused")]
    fn projects_are_not_canonicalized(value: &str) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        assert_eq!(
            discover_for_project(&state, Path::new(value), limits())
                .unwrap_err()
                .to_string(),
            INVALID_PROJECT
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test_case("a*", "b[0]", "axe"; "wildcard_syntax_stays_literal")]
    fn observed_values_are_not_promoted_to_globs(first: &str, second: &str, unobserved: &str) {
        let (_temp, state, mut database) = fixture();
        for (serial, value) in [(1, first), (2, second)] {
            save(
                &mut database,
                serial,
                PROJECT,
                vec![shell(serial, &format!("cargo check -p '{value}'"))],
                vec![],
            );
        }
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        let definition = &report.candidates[0].definition;
        assert!(
            matches!(&definition.slots[0].domain, ArgumentDomain::ObservedSet { values } if values == &BTreeSet::from([first.into(), second.into()]))
        );
        let analysis = analyze_pattern_calls(
            &format!("cargo check -p '{unobserved}'"),
            Path::new(PROJECT),
            Path::new(PROJECT),
            standard_bash_assumptions(),
        )
        .unwrap();
        assert!(
            !CompiledPattern::compile(definition)
                .unwrap()
                .matches(&analysis.observations[0])
                .unwrap()
                .is_match()
        );
    }

    #[test_case("marker"; "history_is_analyzed_never_executed")]
    fn discovery_never_executes_history(marker_name: &str) {
        let (temp, state, mut database) = fixture();
        let marker = temp.path().join(marker_name);
        let command = format!("touch '{}'", marker.display());
        for serial in [1, 2] {
            save(
                &mut database,
                serial,
                PROJECT,
                vec![shell(serial, &command)],
                vec![],
            );
        }
        let report = discover_for_project(&state, Path::new(PROJECT), limits()).unwrap();
        assert!(!report.processing.storage_unavailable);
        assert!(!marker.exists());
    }
}
