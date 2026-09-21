//! Session state, persisted in SQLite.
//!
//! The one exception is `archive/<id>/<seq>.jsonl`: a shrinking save exports
//! the turns it is about to drop so compaction and rewind stay recoverable.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use caudra_workspace::PlanRef;
use tracing::warn;

use crate::id::CaudraId;
use crate::permission_state::PermissionRuleRecord;
use crate::thinking::StoredThinking;
use crate::tool_ledger::{Latency, ToolOutcome};
use crate::workspace_binding::StoredWorkspaceBinding;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{StateDir, StorageError, now_epoch};

#[path = "sessions/database.rs"]
mod database;
#[path = "sessions/lease.rs"]
mod lease;
#[path = "sessions/progress.rs"]
pub mod progress;
#[path = "sessions/sweep.rs"]
pub mod sweep;

pub use database::{
    CheckpointResult, HistoryReadLimits, HistoryReadReport, HistoryRecord,
    HistorySessionReadReport, LedgerEntry, SESSIONS_DB_FILE, SESSIONS_DB_LOCK_FILE, SessionCursor,
    SessionDatabase, SessionRecreation, SessionStorageStats, ToolBucket, ToolLedgerEntry,
    TrimReport, UsageBucket, WAL_RETENTION_LIMIT_BYTES,
};
pub(crate) use database::{from_i64, to_i64};
pub use lease::SessionLease;

const SESSION_VERSION: u32 = 1;
const LOG_FORMAT_VERSION: u32 = 3;
pub const SESSIONS_DIR: &str = "sessions";
const DEFAULT_TITLE: &str = "New session";
pub const DEFAULT_SUBAGENT_PROFILE_NAME: &str = "builtin";
const MAX_TITLE_LEN: usize = 100;
/// Where a shrink rewrite parks the log it is about to drop, as `archive/<id>/`.
pub(crate) const ARCHIVE_DIR: &str = "archive";
/// Archives kept per session. The extra ones go on the next archive, not on a
/// timer.
const ARCHIVE_KEEP: usize = 3;
/// Three copies of a log full of tool output add up fast, so the bytes get a
/// strict budget of their own. A candidate larger than the budget is skipped.
const ARCHIVE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Hands out the token that tags one append-only run of a message list.
/// Process wide, so two runs never pick the same number.
static EPOCH: AtomicU64 = AtomicU64::new(1);

pub fn next_epoch() -> u64 {
    EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// Records that the user opened `id`, which counts as activity for retention.
/// Separate from loading: startup recovery scans and retitling load sessions
/// nobody opened.
pub fn mark_opened(id: CaudraId, dir: &StateDir) -> Result<(), SessionError> {
    SessionDatabase::open(dir)?.mark_opened(id)
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("incompatible session version {found} (expected {expected})")]
    VersionMismatch { found: u32, expected: u32 },
    #[error("session ID mismatch: log owns {log_id}, got {given_id}")]
    IdMismatch {
        log_id: CaudraId,
        given_id: CaudraId,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("session {id} already exists")]
    AlreadyExists { id: CaudraId },
    #[error("session {id} is already open in another Caudra instance")]
    SessionInUse { id: CaudraId },
    #[error(
        "session {id} was modified concurrently: expected write version {expected}, found {actual}"
    )]
    ConcurrentSessionWriter {
        id: CaudraId,
        expected: i64,
        actual: i64,
    },
    #[error("database schema version {found} is unsupported; expected version {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    /// `holders` names the open sessions when lease files identify them, and
    /// says so plainly when they do not: a storage command, a reader, or an
    /// older binary holds the same lock without ever taking a session lease.
    #[error(
        "session database is at schema {found} and must be upgraded to {supported}, \
         which needs exclusive access; {holders}"
    )]
    MigrationBlocked {
        found: i64,
        supported: i64,
        holders: String,
    },
    #[error("invalid database value in {field}: {reason}")]
    CorruptDatabaseValue { field: &'static str, reason: String },
    #[error("{kind} is {actual}, maximum is {maximum}")]
    LimitExceeded {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("session {id} requires {logical_bytes} bytes, eager-load limit is {maximum}")]
    LoadBudgetExceeded {
        id: CaudraId,
        logical_bytes: usize,
        maximum: usize,
    },
    #[error("tool output cleanup failed: {0}")]
    ToolOutputCleanup(#[source] Box<crate::tool_outputs::ToolOutputError>),
    #[error("a session workspace identity cannot be changed after creation")]
    WorkspaceIdentityImmutable,
    #[error("session workspace identity changed; fork or explicitly rebind the session")]
    WorkspaceRebindRequired,
    #[error("remote session cwd must be a bounded logical workspace path")]
    InvalidRemoteCwd,
    #[error("session relocation selection changed; refresh the confirmation")]
    RelocationSelectionChanged,
    #[error("cannot relocate session {id}: {reason}")]
    RelocationBlocked { id: CaudraId, reason: &'static str },
    #[error("session relocation requires an absolute local destination directory")]
    InvalidRelocationDestination,
    #[error("project usage relocation requires an absolute local source directory")]
    InvalidRelocationSource,
    #[error("including project usage requires a full source-directory relocation")]
    ProjectUsageRequiresSource,
    #[error("session relocation requires persistent storage")]
    RelocationUnavailable,
}

/// Per-model token breakdown entry. Mirrors the four usage counters tracked by
/// the active provider; kept storage-local to avoid a circular dependency on
/// `caudra-providers`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_creation: u32,
    #[serde(default)]
    pub cache_read: u32,
    /// What the turns billed, in USD. Prices move (some providers by the hour),
    /// so re-pricing these counters later would be fiction. `None` on unpriced
    /// models, and on entries written before we recorded it until the next load
    /// settles an estimate into them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// The same figure for turns a subscription covered, kept out of `cost`
    /// because no one is invoiced for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_cost: Option<f64>,
}

/// One tool's calls within a session that ended one way. The session-scoped
/// half of the pair `tool_ledger` keeps globally, and the reason `/tools` can
/// still answer for a session after it is resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToolUsage {
    pub tool: String,
    pub source: String,
    pub outcome: ToolOutcome,
    pub calls: u64,
    pub duration_ms: u64,
    pub tokens: u64,
    pub latency: Latency,
}

impl StoredTokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn total(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }

    /// Share of prompt tokens the provider served from its cache. `None` when
    /// nothing was cacheable, so a provider that reports no cache counters
    /// reads as unknown rather than as a perfect miss. A cache write is a miss:
    /// it is a token that had to be sent.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        cache_hit_rate(u64::from(self.cache_read), u64::from(self.total_input()))
    }
}

/// The one definition of a cache hit rate, shared by the session breakdown, the
/// lifetime ledger and the CLI so none of them can disagree.
pub fn cache_hit_rate(cache_read: u64, prompt_tokens: u64) -> Option<f64> {
    (prompt_tokens > 0).then(|| cache_read as f64 / prompt_tokens as f64)
}

impl std::ops::AddAssign for StoredTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_creation = self.cache_creation.saturating_add(rhs.cache_creation);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
        add_cost(&mut self.cost, rhs.cost);
        add_cost(&mut self.subscription_cost, rhs.subscription_cost);
    }
}

/// The one way costs are summed, re-exported by `caudra-providers` so every
/// running total agrees: `None` until the first priced turn shows up, and from
/// there it only grows.
pub fn add_cost(total: &mut Option<f64>, addend: Option<f64>) {
    if let Some(addend) = addend {
        *total = Some(total.unwrap_or_default() + addend);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredGoalVerdict {
    Met,
    NotMet,
    Impossible,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredGoalResult {
    pub condition: String,
    pub verdict: StoredGoalVerdict,
    pub reason: String,
    pub evaluations: u32,
    pub duration_ms: u64,
    #[serde(default)]
    pub usage: StoredTokenUsage,
}

/// A goal still running when the session was last written. Everything the
/// status panel shows, because a goal that survives a resume and reports zero
/// spend and zero evaluations is worse than one that reports nothing at all.
///
/// `elapsed_ms` accumulates rather than recording a start instant: the session
/// is not running between resumes, and billing a goal for the days it spent
/// closed would be a lie in the other direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredActiveGoal {
    pub condition: String,
    #[serde(default)]
    pub evaluations: u32,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub usage: StoredTokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verdict: Option<StoredGoalVerdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPasteRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredImage {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredQueuedDraft {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paste_ranges: Vec<StoredPasteRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredQueuedPrompt {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<StoredImage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paste_ranges: Vec<StoredPasteRange>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredPromptAdmission {
    #[default]
    Queue,
    Steer,
    Interrupt,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_head: Option<CaudraId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_revert: Option<PendingConversationRevert>,
    /// `None` until the session picks a mode, which is what lets a fresh
    /// session take the front end's default instead of a stored choice.
    #[serde(default)]
    pub mode: Option<StoredMode>,
    #[serde(default)]
    pub plan_path: Option<String>,
    #[serde(default)]
    pub plan_target: Option<StoredPlanTarget>,
    #[serde(default)]
    pub plan_written: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structured_permission_rules: Vec<PermissionRuleRecord>,
    #[serde(skip)]
    pub permission_generation: u64,
    #[serde(default)]
    pub context_size: u32,
    /// Completed exchanges over the session's whole life. Counted rather than
    /// derived because compaction replaces the history it would be derived
    /// from.
    #[serde(default)]
    pub turns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_draft: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_draft_images: Vec<StoredImage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_draft_pastes: Vec<StoredPasteRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<StoredQueuedPrompt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_message_admissions: Vec<StoredPromptAdmission>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queued_messages_together: bool,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub unsent_subagent_messages: HashMap<String, Vec<StoredQueuedDraft>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<StoredThinking>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_goal: Option<Box<StoredActiveGoal>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_result: Option<Box<StoredGoalResult>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_continuation_limit: Option<u32>,
    /// `None` when the user never set yolo for this session, which is what
    /// makes `--yolo` a property of the invocation rather than of the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    /// Why this workspace has no file revert, once something decided so. Kept
    /// on the session because the verdict is worth reporting after a restart
    /// and is not worth re-deciding by walking the tree again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots_unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingConversationRevert {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_head: Option<CaudraId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_head: Option<CaudraId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_workspace_head: Option<StoredHistoryHead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_head: Option<StoredHistoryHead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_status: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_operation: Option<PendingRestoreOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredHistoryHead {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<CaudraId>,
}

impl From<Option<CaudraId>> for StoredHistoryHead {
    fn from(head: Option<CaudraId>) -> Self {
        Self { head }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingRestoreKind {
    Revert,
    Unrevert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingRestorePhase {
    Intent,
    FilesApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRestoreOperation {
    pub id: CaudraId,
    pub kind: PendingRestoreKind,
    pub phase: PendingRestorePhase,
    pub target_workspace_head: StoredHistoryHead,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_target: Option<StoredHistoryHead>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overwrite: bool,
}

/// Messages plus the token of the run they belong to. Comparing tokens tells
/// an append from a rewrite, with no need to diff the lists.
#[derive(Clone)]
pub struct HistorySnapshot<M> {
    pub epoch: u64,
    pub messages: Arc<Vec<M>>,
}

impl<M> HistorySnapshot<M> {
    pub fn new(messages: Vec<M>) -> Self {
        Self {
            epoch: next_epoch(),
            messages: Arc::new(messages),
        }
    }
}

impl<M> Default for HistorySnapshot<M> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// The conversation collections are private so every change goes through a
/// mutator that classifies itself: `revision` says "this needs writing",
/// `epoch` says append cursors are void, and `rewrites` identifies replacement
/// of existing canonical values. `cwd` and `model` go through setters because
/// changing either also invalidates append classification.
///
/// [`SessionMeta`] is the part the owner mirrors from its own live state and
/// hands over whole on every checkpoint. Whatever the session maintains itself
/// gets a field of its own instead, so a checkpoint never copies it out and
/// back in only to compare it against itself.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session<M, U, T> {
    pub version: u32,
    pub id: CaudraId,
    pub title: String,
    pub cwd: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_binding: Option<Box<StoredWorkspaceBinding>>,
    messages: Arc<Vec<M>>,
    pub token_usage: U,
    #[serde(default = "HashMap::new")]
    tool_outputs: HashMap<String, Arc<T>>,
    #[serde(default = "HashMap::new", skip_serializing_if = "HashMap::is_empty")]
    subagent_messages: HashMap<String, Arc<Vec<M>>>,
    #[serde(default = "HashMap::new", skip_serializing_if = "HashMap::is_empty")]
    subagent_task_specs: HashMap<String, StoredSubagentTaskSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    subagents: Vec<StoredSubagent>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    usage_by_model: HashMap<String, StoredTokenUsage>,
    /// A list rather than a map because the key is a triple, and because this
    /// is rewritten whole on every save anyway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_usage: Vec<StoredToolUsage>,
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub created_at: u64,
    pub updated_at: u64,
    /// Bumped by every mutation, so a checkpoint knows if there is anything
    /// to write.
    #[serde(skip)]
    revision: u64,
    /// Bumped by every mutation except `meta`, so a checkpoint can tell a tool
    /// result, which has to reach disk now, from a keystroke in the draft,
    /// which can wait for the keystrokes behind it.
    #[serde(skip)]
    content_revision: u64,
    /// The append-only run `messages` belongs to, adopted from the producer's
    /// snapshot or minted fresh when this session rewrites them itself. Once
    /// it changes, every append cursor into the log is void.
    #[serde(skip, default = "next_epoch")]
    epoch: u64,
    /// Bumped when this session rewrites a collection in place (replaced
    /// messages, tool outputs or subagent histories). Kept apart from
    /// `epoch` so `set_history` adopting a producer's snapshot can never
    /// erase a locally minted void: cursor validity is the pair.
    #[serde(skip)]
    rewrites: u64,
    /// Frozen expected database version for this snapshot. Clones capture the
    /// latest committed value from the shared atomic, while a clone already in
    /// flight keeps its base so a later commit makes that snapshot stale.
    #[serde(skip, default = "default_write_version")]
    base_write_version: AtomicI64,
    /// Communicates successful commits only to snapshots cloned afterwards; it
    /// is not itself the expected version of every existing clone.
    #[serde(skip, default = "new_write_version")]
    write_version: Arc<AtomicI64>,
}

impl<M: Clone, U: Clone, T: Clone> Clone for Session<M, U, T> {
    fn clone(&self) -> Self {
        let write_version = self.write_version.load(Ordering::Acquire);
        Self {
            version: self.version,
            id: self.id,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            workspace_binding: self.workspace_binding.clone(),
            messages: Arc::clone(&self.messages),
            token_usage: self.token_usage.clone(),
            tool_outputs: self.tool_outputs.clone(),
            subagent_messages: self.subagent_messages.clone(),
            subagent_task_specs: self.subagent_task_specs.clone(),
            subagents: self.subagents.clone(),
            usage_by_model: self.usage_by_model.clone(),
            tool_usage: self.tool_usage.clone(),
            meta: self.meta.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            revision: self.revision,
            content_revision: self.content_revision,
            epoch: self.epoch,
            rewrites: self.rewrites,
            base_write_version: AtomicI64::new(write_version),
            write_version: Arc::clone(&self.write_version),
        }
    }
}

fn default_write_version() -> AtomicI64 {
    AtomicI64::new(-1)
}

fn new_write_version() -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(-1))
}

#[derive(Serialize)]
pub struct SessionSummary {
    pub id: CaudraId,
    pub title: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLocation {
    pub id: CaudraId,
    pub title: String,
    pub cwd: String,
    pub updated_at: u64,
    pub write_version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRelocation {
    pub sessions: Vec<SessionLocation>,
    pub source_cwd: Option<String>,
    pub destination: String,
    pub include_project_usage: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectUsageRelocation {
    pub buckets_moved: usize,
    pub buckets_merged: usize,
    pub tool_buckets_moved: usize,
    pub tool_buckets_merged: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionRelocationResult {
    pub sessions_moved: usize,
    pub project_usage: Option<ProjectUsageRelocation>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredMode {
    #[default]
    Build,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredPlanTarget {
    LocalPath { path: String },
    PlanRef { reference: PlanRef },
}

impl fmt::Display for StoredMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build => f.write_str("build"),
            Self::Plan => f.write_str("plan"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSubagentTaskSpec {
    #[serde(default, skip_serializing_if = "StoredSubagentKind::is_task")]
    pub kind: StoredSubagentKind,
    #[serde(default = "default_subagent_profile_name")]
    pub profile_name: String,
    #[serde(default)]
    pub mode: StoredMode,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredSubagentKind {
    #[default]
    Task,
    Generic,
    Version,
}

impl StoredSubagentKind {
    fn is_task(&self) -> bool {
        *self == Self::Task
    }
}

impl Default for StoredSubagentTaskSpec {
    fn default() -> Self {
        Self {
            kind: StoredSubagentKind::Task,
            profile_name: default_subagent_profile_name(),
            mode: StoredMode::default(),
        }
    }
}

impl StoredSubagentTaskSpec {
    pub fn generic() -> Self {
        Self {
            kind: StoredSubagentKind::Generic,
            ..Self::default()
        }
    }

    pub fn is_generic(&self) -> bool {
        self.kind == StoredSubagentKind::Generic
    }

    pub fn version() -> Self {
        Self {
            kind: StoredSubagentKind::Version,
            ..Self::default()
        }
    }

    pub fn is_version(&self) -> bool {
        self.kind == StoredSubagentKind::Version
    }
}

fn default_subagent_profile_name() -> String {
    DEFAULT_SUBAGENT_PROFILE_NAME.to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSubagent {
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_tool_use_id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning level this task's own requests carried, as the subagent
    /// resolved it against its own model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    pub outcome: StoredSubagentOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredSubagentOutcome {
    Unknown,
    Done,
    Killed,
    Error,
}

impl StoredSubagentOutcome {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Done => "done",
            Self::Killed => "killed",
            Self::Error => "error",
        }
    }

    pub(crate) fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "unknown" => Some(Self::Unknown),
            "done" => Some(Self::Done),
            "killed" => Some(Self::Killed),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

pub trait TitleSource {
    fn first_user_text(&self) -> Option<&str>;
}

/// A pasted code block bakes `\n` into a title and skews width-based padding
/// in single-line UI like the picker, so every title entry point calls this.
pub fn normalize_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn generate_title<M: TitleSource>(messages: &[M]) -> String {
    let first_user_text = messages.iter().find_map(|m| m.first_user_text());

    let Some(text) = first_user_text.map(str::trim).filter(|t| !t.is_empty()) else {
        return DEFAULT_TITLE.into();
    };
    truncate_title(&normalize_title(text))
}

/// Shared by the heuristic title and the model-written one, so both render the
/// same way in the picker.
pub fn truncate_title(text: &str) -> String {
    if text.len() <= MAX_TITLE_LEN {
        return text.to_owned();
    }

    let boundary = text.floor_char_boundary(MAX_TITLE_LEN);
    let truncated = &text[..boundary];
    match truncated.rfind(' ') {
        Some(pos) if pos > MAX_TITLE_LEN / 2 => format!("{}…", &truncated[..pos]),
        _ => format!("{truncated}…"),
    }
}

// -- JSONL record types --

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum LogRecord<M, U, T> {
    #[serde(rename = "header")]
    Header {
        v: u32,
        id: CaudraId,
        model: String,
        cwd: String,
        created_at: u64,
    },
    #[serde(rename = "msg")]
    Msg { d: M },
    #[serde(rename = "out")]
    Out { id: String, d: T },
    #[serde(rename = "sub_msg")]
    SubMsg { sub: String, d: M },
    #[serde(rename = "meta")]
    Meta {
        title: String,
        token_usage: U,
        updated_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_binding: Option<Box<StoredWorkspaceBinding>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        subagents: Vec<StoredSubagent>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        usage_by_model: HashMap<String, StoredTokenUsage>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        subagent_task_specs: HashMap<String, StoredSubagentTaskSpec>,
        #[serde(flatten)]
        meta: Box<SessionMeta>,
    },
}

/// `<seq>.jsonl`, counting up. A number cannot step back the way a clock does
/// after an NTP fix or a suspend, so the order is always the truth and pruning
/// can never mistake the newest archive for the oldest. The mtime says when.
struct Archive {
    seq: u64,
    size: u64,
    path: PathBuf,
}

/// Newest first: the next name comes off the front, pruning walks to the back.
fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

fn archives_newest_first(archive_dir: &Path) -> Vec<Archive> {
    let Ok(entries) = fs::read_dir(archive_dir) else {
        return Vec::new();
    };
    let mut archives: Vec<Archive> = entries
        .flatten()
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let path = entry.path();
            if !is_jsonl(&path) {
                return None;
            }
            Some(Archive {
                seq: path.file_stem()?.to_str()?.parse().ok()?,
                size: entry.metadata().ok()?.len(),
                path,
            })
        })
        .collect();
    archives.sort_unstable_by_key(|a| Reverse(a.seq));
    archives
}

/// Walks from the newest and keeps what both budgets allow, so the rest go.
/// `new_bytes` is already known to fit the strict byte budget. It is not in
/// `existing`, so pruning only decides which older recovery points still fit.
fn prune_archives(existing: Vec<Archive>, new_bytes: u64) -> Result<(), std::io::Error> {
    let mut total = new_bytes;
    let mut room = ARCHIVE_KEEP.saturating_sub(1);
    for archive in existing {
        total += archive.size;
        if room > 0 && total <= ARCHIVE_MAX_BYTES {
            room -= 1;
            continue;
        }
        fs::remove_file(&archive.path)?;
    }
    Ok(())
}

fn prune_archives_to_budget(archives: Vec<Archive>) -> Result<(), std::io::Error> {
    let mut total = 0u64;
    let mut room = ARCHIVE_KEEP;
    for archive in archives {
        if room > 0 && total.saturating_add(archive.size) <= ARCHIVE_MAX_BYTES {
            room -= 1;
            total += archive.size;
        } else {
            fs::remove_file(&archive.path)?;
        }
    }
    Ok(())
}

fn meta_record<M, U, T>(session: &Session<M, U, T>) -> Result<Vec<u8>, SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    append_record(
        &mut buf,
        &LogRecord::<&M, &U, &T>::Meta {
            title: session.title.clone(),
            token_usage: &session.token_usage,
            updated_at: session.updated_at,
            workspace_binding: session.workspace_binding.clone(),
            subagents: session.subagents.clone(),
            usage_by_model: session.usage_by_model.clone(),
            subagent_task_specs: session.subagent_task_specs.clone(),
            meta: Box::new(session.meta.clone()),
        },
    )?;
    Ok(buf)
}

fn write_full_session<M, U, T>(
    file: &mut File,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    append_record(
        &mut buf,
        &LogRecord::<&M, &U, &T>::Header {
            v: LOG_FORMAT_VERSION,
            id: session.id,
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            created_at: session.created_at,
        },
    )?;
    for msg in session.messages.iter() {
        append_record(&mut buf, &LogRecord::<&M, &U, &T>::Msg { d: msg })?;
    }
    for (id, output) in &session.tool_outputs {
        append_record(
            &mut buf,
            &LogRecord::<&M, &U, &T>::Out {
                id: id.clone(),
                d: output,
            },
        )?;
    }
    for (sub_id, msgs) in &session.subagent_messages {
        for msg in msgs.iter() {
            append_record(
                &mut buf,
                &LogRecord::<&M, &U, &T>::SubMsg {
                    sub: sub_id.clone(),
                    d: msg,
                },
            )?;
        }
    }
    buf.extend_from_slice(&meta_record(session)?);
    file.write_all(&buf).map_err(StorageError::from)?;
    Ok(())
}

fn append_record<R: Serialize>(buf: &mut Vec<u8>, record: &R) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(StorageError::from)?;
    buf.push(b'\n');
    Ok(())
}

pub fn persisted_session_ids(dir: &StateDir) -> Result<Vec<CaudraId>, SessionError> {
    SessionDatabase::open(dir)?.persisted_session_ids()
}

impl<M, U, T> Session<M, U, T>
where
    M: Serialize + DeserializeOwned + TitleSource + Clone + Send,
    U: Serialize + DeserializeOwned + Default + Send,
    T: Serialize + DeserializeOwned + Send,
{
    pub fn new(model: &str, cwd: &str) -> Self {
        Self::new_with_workspace(model, cwd, StoredWorkspaceBinding::local_from_cwd(cwd))
    }

    pub fn new_with_workspace(
        model: &str,
        cwd: &str,
        workspace_binding: StoredWorkspaceBinding,
    ) -> Self {
        let now = now_epoch();
        Self {
            version: SESSION_VERSION,
            id: CaudraId::generate(),
            title: DEFAULT_TITLE.into(),
            cwd: cwd.into(),
            model: model.into(),
            workspace_binding: Some(Box::new(workspace_binding)),
            messages: Arc::default(),
            token_usage: U::default(),
            tool_outputs: HashMap::new(),
            subagent_messages: HashMap::new(),
            subagent_task_specs: HashMap::new(),
            subagents: Vec::new(),
            usage_by_model: HashMap::new(),
            tool_usage: Vec::new(),
            meta: SessionMeta::default(),
            created_at: now,
            updated_at: now,
            revision: 0,
            content_revision: 0,
            epoch: next_epoch(),
            rewrites: 0,
            base_write_version: AtomicI64::new(-1),
            write_version: new_write_version(),
        }
    }

    pub fn messages(&self) -> &[M] {
        &self.messages
    }

    pub fn take_messages(self) -> Vec<M> {
        Arc::unwrap_or_clone(self.messages)
    }

    pub fn tool_outputs(&self) -> &HashMap<String, Arc<T>> {
        &self.tool_outputs
    }

    pub fn subagent_messages(&self) -> &HashMap<String, Arc<Vec<M>>> {
        &self.subagent_messages
    }

    pub fn subagent_task_specs(&self) -> &HashMap<String, StoredSubagentTaskSpec> {
        &self.subagent_task_specs
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn content_revision(&self) -> u64 {
        self.content_revision
    }

    pub fn persisted_write_version(&self) -> Option<i64> {
        let version = self.base_write_version.load(Ordering::Acquire);
        (version >= 0).then_some(version)
    }

    pub fn workspace_binding(&self) -> Option<&StoredWorkspaceBinding> {
        self.workspace_binding.as_deref()
    }

    pub fn set_persisted_write_version(&mut self, version: Option<i64>) {
        let version = version.unwrap_or(-1);
        self.base_write_version.store(version, Ordering::Release);
        self.write_version.store(version, Ordering::Release);
    }

    /// A serialized writer may adopt its own latest commit for a newer queued
    /// snapshot that was cloned while the preceding write was still in flight.
    pub fn adopt_persisted_write_version(&self, version: i64) {
        self.base_write_version.store(version, Ordering::Release);
    }

    fn touch(&mut self) {
        self.content_revision += 1;
        self.touch_soft();
    }

    /// Only UI state moved, so the write can wait for company. Everything a
    /// crash would lose for good goes through `touch`, which is the default a
    /// new mutator gets by not thinking about it.
    fn touch_soft(&mut self) {
        self.updated_at = now_epoch();
        self.revision += 1;
    }

    /// Every append cursor into the log is void from here on. Counted in
    /// `rewrites`, which snapshot adoption never touches, so a same-frame
    /// `set_history` cannot erase the void before the writer sees it.
    fn rewrite(&mut self) {
        self.rewrites += 1;
        self.touch();
    }

    /// [`Self::rewrite`] for local changes to `messages`: they also leave the
    /// producer's run, so the epoch is minted fresh. Once this state is
    /// saved, re-adopting a stale run snapshot keeps diverging instead of
    /// splicing its tail onto a rewound log.
    fn rewrite_messages(&mut self) {
        self.epoch = next_epoch();
        self.rewrite();
    }

    pub fn push_message(&mut self, msg: M) {
        Arc::make_mut(&mut self.messages).push(msg);
        self.touch();
    }

    pub fn replace_messages(&mut self, messages: Vec<M>) {
        self.messages = Arc::new(messages);
        self.rewrite_messages();
    }

    /// Installs an all-node merge of a producer's active history. Appended
    /// nodes retain the producer epoch; changed existing nodes force a rewrite.
    pub fn merge_history(&mut self, snapshot: &HistorySnapshot<M>, messages: Vec<M>)
    where
        M: PartialEq,
    {
        if self.messages.as_slice() == messages.as_slice() {
            return;
        }
        let append_only = messages.starts_with(self.messages.as_slice());
        self.messages = Arc::new(messages);
        self.epoch = snapshot.epoch;
        if append_only {
            self.touch();
        } else {
            self.rewrite();
        }
    }

    pub fn truncate_messages(&mut self, len: usize) {
        if len >= self.messages.len() {
            return;
        }
        Arc::make_mut(&mut self.messages).truncate(len);
        self.rewrite_messages();
    }

    /// Adopting a producer's snapshot inherits its run token, so the log's
    /// cursors survive exactly when the snapshot was an append.
    fn set_history(&mut self, snapshot: &HistorySnapshot<M>) {
        self.messages = Arc::clone(&snapshot.messages);
        self.epoch = snapshot.epoch;
        self.touch();
    }

    /// Applies everything the owner mirrors from live state. It takes an `Arc`
    /// and checks for a real change first because `Arc::make_mut` deep-copies
    /// the whole session while the writer still holds the last snapshot, and an
    /// idle session should not pay for that every frame.
    pub fn checkpoint(
        this: &mut Arc<Self>,
        history: Option<&HistorySnapshot<M>>,
        meta: SessionMeta,
        token_usage: U,
    ) where
        M: Clone,
        U: PartialEq + Clone,
        T: Clone,
    {
        let history = history.filter(|h| !Arc::ptr_eq(&this.messages, &h.messages));
        if history.is_none() && this.meta == meta && this.token_usage == token_usage {
            return;
        }
        let session = Arc::make_mut(this);
        if let Some(snapshot) = history {
            session.set_history(snapshot);
            // The title comes from the messages, so it goes stale exactly when
            // they move.
            session.update_title_if_default();
        }
        session.set_meta(meta);
        session.set_token_usage(token_usage);
    }

    /// A change under an existing id is not expressible as an append, so it
    /// voids the cursors; a new id is a pure append.
    pub fn insert_tool_output(&mut self, id: String, output: T) {
        if self.tool_outputs.insert(id, Arc::new(output)).is_some() {
            self.rewrite();
        } else {
            self.touch();
        }
    }

    pub fn set_subagent_messages(&mut self, id: String, msgs: Vec<M>) {
        let spec = self.subagent_task_specs.get(&id).cloned();
        self.set_subagent_history(id, msgs, spec);
    }

    pub fn set_subagent_history(
        &mut self,
        id: String,
        msgs: Vec<M>,
        spec: Option<StoredSubagentTaskSpec>,
    ) {
        let append_only = self.subagent_messages.get(&id).is_none_or(|stored| {
            let Some(prefix) = msgs.get(..stored.len()) else {
                return false;
            };
            match (
                serde_json::to_vec(stored.as_slice()),
                serde_json::to_vec(prefix),
            ) {
                (Ok(stored), Ok(prefix)) => stored == prefix,
                _ => false,
            }
        });
        let changed = self
            .subagent_messages
            .get(&id)
            .is_none_or(|stored| stored.len() != msgs.len() || !append_only);
        let previous_spec = self.subagent_task_specs.get(&id);
        let spec_changed = previous_spec != spec.as_ref();
        self.subagent_messages.insert(id.clone(), Arc::new(msgs));
        if let Some(spec) = spec {
            self.subagent_task_specs.insert(id, spec);
        } else {
            self.subagent_task_specs.remove(&id);
        }
        if !changed && !spec_changed {
            return;
        }
        if append_only {
            self.touch();
        } else {
            self.rewrite();
        }
    }

    fn set_token_usage(&mut self, usage: U)
    where
        U: PartialEq,
    {
        if self.token_usage == usage {
            return;
        }
        self.token_usage = usage;
        self.touch();
    }

    fn set_meta(&mut self, meta: SessionMeta) {
        if self.meta == meta {
            return;
        }
        self.meta = meta;
        self.touch_soft();
    }

    pub fn set_workspace_binding(
        &mut self,
        binding: StoredWorkspaceBinding,
    ) -> Result<(), SessionError> {
        match self.workspace_binding.as_deref() {
            Some(current) if !current.same_workspace_identity(&binding) => {
                Err(SessionError::WorkspaceIdentityImmutable)
            }
            Some(_) => Ok(()),
            None => {
                self.workspace_binding = Some(Box::new(binding));
                self.touch();
                Ok(())
            }
        }
    }

    pub fn replace_workspace_cursor(
        &mut self,
        mut binding: StoredWorkspaceBinding,
    ) -> Result<(), SessionError> {
        if let Some(record) = self
            .workspace_binding
            .as_ref()
            .and_then(|binding| binding.sandbox_record())
        {
            binding = binding
                .with_sandbox_record(record)
                .map_err(|_| SessionError::WorkspaceIdentityImmutable)?;
        }
        match self.workspace_binding.as_deref() {
            Some(current) if !current.same_workspace_identity(&binding) => {
                Err(SessionError::WorkspaceIdentityImmutable)
            }
            Some(current) if current == &binding => Ok(()),
            Some(_) => {
                self.workspace_binding = Some(Box::new(binding));
                self.touch();
                Ok(())
            }
            None => self.set_workspace_binding(binding),
        }
    }

    pub fn set_conversation_state(
        &mut self,
        history_head: Option<CaudraId>,
        pending_revert: Option<PendingConversationRevert>,
    ) {
        if self.meta.history_head == history_head && self.meta.pending_revert == pending_revert {
            return;
        }
        self.meta.history_head = history_head;
        self.meta.pending_revert = pending_revert;
        self.touch();
    }

    pub fn subagents(&self) -> &[StoredSubagent] {
        &self.subagents
    }

    pub fn set_subagents(&mut self, subagents: Vec<StoredSubagent>) {
        if self.subagents == subagents {
            return;
        }
        self.subagents = subagents;
        self.touch();
    }

    pub fn usage_by_model(&self) -> &HashMap<String, StoredTokenUsage> {
        &self.usage_by_model
    }

    /// For settling costs on load; every other write goes through
    /// [`Self::add_model_usage`].
    pub fn usage_by_model_mut(&mut self) -> &mut HashMap<String, StoredTokenUsage> {
        self.touch();
        &mut self.usage_by_model
    }

    pub fn set_title(&mut self, title: String) {
        if self.title == title {
            return;
        }
        self.title = title;
        self.touch();
    }

    /// Header field: appends never rewrite the header, so the change voids
    /// the cursors to force a full rewrite.
    pub fn set_cwd(&mut self, cwd: String) {
        if self.cwd == cwd {
            return;
        }
        self.cwd = cwd;
        self.rewrite();
    }

    /// Header field, see [`Self::set_cwd`].
    pub fn set_model(&mut self, model: String) {
        if self.model == model {
            return;
        }
        self.model = model;
        self.rewrite();
    }

    pub fn add_model_usage(&mut self, model: &str, usage: StoredTokenUsage) {
        *self.usage_by_model.entry(model.to_owned()).or_default() += usage;
        self.touch();
    }

    pub fn tool_usage(&self) -> &[StoredToolUsage] {
        &self.tool_usage
    }

    /// Folds one finished call into the row for its tool, source and outcome.
    /// Linear over a list that holds one entry per tool per outcome, which is
    /// tens of entries in a long session and cheaper than hashing a triple.
    pub fn add_tool_usage(
        &mut self,
        tool: &str,
        source: &str,
        outcome: ToolOutcome,
        duration_ms: u64,
        tokens: u32,
    ) {
        let tokens = u64::from(tokens);
        match self
            .tool_usage
            .iter_mut()
            .find(|entry| entry.tool == tool && entry.source == source && entry.outcome == outcome)
        {
            Some(entry) => {
                entry.calls = entry.calls.saturating_add(1);
                entry.duration_ms = entry.duration_ms.saturating_add(duration_ms);
                entry.tokens = entry.tokens.saturating_add(tokens);
                entry.latency.record(duration_ms);
            }
            None => self.tool_usage.push(StoredToolUsage {
                tool: tool.to_owned(),
                source: source.to_owned(),
                outcome,
                calls: 1,
                duration_ms,
                tokens,
                latency: Latency::of(duration_ms),
            }),
        }
        self.touch();
    }

    /// After `messages` is truncated (rewind), state keyed by tool_use_id can
    /// point at calls that no longer exist. On restore that shows up as ghost
    /// subagent tabs and leaked tool outputs, so this drops everything not
    /// reachable from `messages`.
    ///
    /// If you add another field keyed by tool_use_id, prune it here too.
    pub fn prune_orphans(&mut self, tool_ids: impl Fn(&M) -> Vec<String>) {
        let main_ids: HashSet<String> = self.messages.iter().flat_map(&tool_ids).collect();
        self.subagent_messages.retain(|id, _| main_ids.contains(id));
        self.subagent_task_specs
            .retain(|id, _| self.subagent_messages.contains_key(id));
        self.subagents
            .retain(|sa| main_ids.contains(&sa.tool_use_id));

        let live: HashSet<String> = self
            .subagent_messages
            .values()
            .flat_map(|msgs| msgs.iter())
            .flat_map(&tool_ids)
            .chain(main_ids)
            .collect();
        self.tool_outputs.retain(|id, _| live.contains(id));
        self.rewrite();
    }

    pub fn save(&mut self, dir: &StateDir) -> Result<(), SessionError> {
        self.updated_at = now_epoch();
        let cursor = SessionDatabase::open(dir)?.save(self, None)?;
        self.set_persisted_write_version(Some(cursor.write_version()));
        Ok(())
    }

    pub fn load(id: CaudraId, dir: &StateDir) -> Result<Self, SessionError> {
        SessionDatabase::open(dir)?.load(id)
    }

    pub fn list(cwd: &str, dir: &StateDir) -> Result<Vec<SessionSummary>, SessionError> {
        SessionDatabase::open(dir)?.list(cwd)
    }

    pub fn list_for_workspace(
        binding: &StoredWorkspaceBinding,
        dir: &StateDir,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        SessionDatabase::open(dir)?.list_for_workspace(binding)
    }

    pub fn latest(cwd: &str, dir: &StateDir) -> Result<Option<Self>, SessionError> {
        let database = SessionDatabase::open(dir)?;
        database
            .latest_id(cwd)?
            .map(|id| database.load(id))
            .transpose()
    }

    pub fn latest_for_workspace(
        binding: &StoredWorkspaceBinding,
        dir: &StateDir,
    ) -> Result<Option<Self>, SessionError> {
        let database = SessionDatabase::open(dir)?;
        database
            .latest_id_for_workspace(binding)?
            .map(|id| database.load(id))
            .transpose()
    }

    pub fn update_title_if_default(&mut self) {
        if self.title == DEFAULT_TITLE {
            self.set_title(generate_title(&self.messages));
        }
    }

    /// A model-written title may land long after the prompt that triggered it,
    /// by which time the user could have renamed the session or forked it.
    /// Only a title Caudra derived itself is safe to replace, and that is
    /// exactly the one [`generate_title`] still reproduces.
    pub fn set_title_if_auto(&mut self, title: String) {
        if self.title == DEFAULT_TITLE || self.title == generate_title(&self.messages) {
            self.set_title(title);
        }
    }

    pub fn delete(id: CaudraId, dir: &StateDir) -> Result<(), SessionError> {
        Self::delete_with_version(id, dir, None)
    }

    pub fn delete_with_version(
        id: CaudraId,
        dir: &StateDir,
        expected_write_version: Option<i64>,
    ) -> Result<(), SessionError> {
        let recreation = Self::delete_impl(id, dir, expected_write_version)?;
        if !recreation.removed_existing() {
            return Err(StorageError::NotFound(id.to_string()).into());
        }
        Ok(())
    }

    pub fn delete_for_recreation(
        id: CaudraId,
        dir: &StateDir,
        expected_write_version: Option<i64>,
    ) -> Result<SessionRecreation, SessionError> {
        Self::delete_impl(id, dir, expected_write_version)
    }

    fn delete_impl(
        id: CaudraId,
        dir: &StateDir,
        expected_write_version: Option<i64>,
    ) -> Result<SessionRecreation, SessionError> {
        let mut database = SessionDatabase::open(dir)?;
        let result = database.delete(id, expected_write_version);
        if let Err(error) = database.process_cleanup_jobs() {
            warn!(%error, %id, "session artifact cleanup deferred");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::StoredThinking;
    use super::{
        ARCHIVE_DIR, ARCHIVE_KEEP, ARCHIVE_MAX_BYTES, DEFAULT_TITLE, LOG_FORMAT_VERSION,
        MAX_TITLE_LEN, SESSION_VERSION, SESSIONS_DIR, StoredImage, StoredMode, StoredPasteRange,
        StoredPromptAdmission, StoredQueuedDraft, StoredQueuedPrompt, StoredSubagent,
        StoredSubagentOutcome, StoredSubagentTaskSpec, StoredTokenUsage, generate_title,
        meta_record, next_epoch, persisted_session_ids, write_full_session,
    };
    use super::{
        HistorySnapshot, PendingConversationRevert, Session, SessionCursor, SessionDatabase,
        SessionError, SessionMeta, StorageError, TitleSource,
    };
    use crate::StateDir;
    use crate::id::CaudraId;
    use crate::tool_outputs::{ToolOutputError, ToolOutputStore};
    use serde_json::Value;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::TempDir;
    use test_case::test_case;

    type TestSession = Session<Value, Value, Value>;

    const SONNET_COST: f64 = 0.42;
    const HAIKU_COST: f64 = 0.08;
    const PENDING_DRAFT: &str = "half typed thought";
    /// Two of these already break the byte budget.
    const FAKE_ARCHIVE_BYTES: u64 = ARCHIVE_MAX_BYTES / 2;
    const EXISTING_ARCHIVE_SEQ: u64 = 7;
    const SUBAGENT_PROFILE: &str = "review";
    const HEADER_RECORD: &str = "header";
    const MSG_RECORD: &str = "msg";
    const META_RECORD: &str = "meta";
    const LONG_TITLE: &str = "This is a very long title that exceeds the one hundred character cap and should therefore be truncated at a word boundary";
    const LONG_TITLE_TRUNCATED: &str = "This is a very long title that exceeds the one hundred character cap and should therefore be…";
    const MODEL_TITLE: &str = "Session title from a small model";
    const RENAMED_TITLE: &str = "Renamed by hand";
    const TITLE_PROMPT: &str = "add refresh token support";
    const FORK_TITLE: &str = "Renamed by hand (fork #1)";
    const MODE_UNCHOSEN: &str = "a new session must not pretend it picked a mode";

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, state_dir)
    }

    fn subagent_spec() -> StoredSubagentTaskSpec {
        StoredSubagentTaskSpec {
            profile_name: SUBAGENT_PROFILE.into(),
            mode: StoredMode::Plan,
            ..StoredSubagentTaskSpec::default()
        }
    }

    #[test]
    fn subagent_task_spec_fields_have_legacy_defaults() {
        let default: StoredSubagentTaskSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(default, StoredSubagentTaskSpec::default());

        let without_mode: StoredSubagentTaskSpec =
            serde_json::from_value(serde_json::json!({"profile_name": SUBAGENT_PROFILE})).unwrap();
        assert_eq!(without_mode.profile_name, SUBAGENT_PROFILE);
        assert_eq!(without_mode.mode, StoredMode::Build);

        let without_profile: StoredSubagentTaskSpec =
            serde_json::from_value(serde_json::json!({"mode": "plan"})).unwrap();
        assert_eq!(
            without_profile.profile_name,
            super::DEFAULT_SUBAGENT_PROFILE_NAME
        );
        assert_eq!(without_profile.mode, StoredMode::Plan);
    }

    #[test_case(StoredSubagentOutcome::Unknown, r#""unknown""# ; "unknown")]
    #[test_case(StoredSubagentOutcome::Done, r#""done""# ; "done")]
    #[test_case(StoredSubagentOutcome::Killed, r#""killed""# ; "killed")]
    #[test_case(StoredSubagentOutcome::Error, r#""error""# ; "error")]
    fn subagent_outcome_has_a_compact_serde_representation(
        outcome: StoredSubagentOutcome,
        expected: &str,
    ) {
        assert_eq!(serde_json::to_string(&outcome).unwrap(), expected);
        assert_eq!(
            serde_json::from_str::<StoredSubagentOutcome>(expected).unwrap(),
            outcome
        );
    }

    #[test]
    fn old_session_meta_defaults_paste_ranges() {
        let meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "input_draft": "plain draft"
        }))
        .unwrap();
        assert_eq!(meta.input_draft.as_deref(), Some("plain draft"));
        assert!(meta.input_draft_pastes.is_empty());
        assert!(meta.input_draft_images.is_empty());
        assert_eq!(meta.history_head, None);
        assert_eq!(meta.pending_revert, None);
        assert!(meta.structured_permission_rules.is_empty());
    }

    #[test]
    fn old_serialized_session_without_workspace_binding_still_loads() {
        let session = TestSession::new("model", "/legacy/cwd");
        let mut value = serde_json::to_value(&session).unwrap();
        value.as_object_mut().unwrap().remove("workspace_binding");

        let restored: TestSession = serde_json::from_value(value).unwrap();

        assert_eq!(restored.cwd, "/legacy/cwd");
        assert_eq!(restored.workspace_binding(), None);
    }

    #[test]
    fn session_meta_roundtrips_paste_ranges() {
        let meta = SessionMeta {
            input_draft: Some("a\nb\nc".into()),
            input_draft_pastes: vec![StoredPasteRange { start: 0, end: 5 }],
            ..SessionMeta::default()
        };
        let value = serde_json::to_value(&meta).unwrap();
        let restored: SessionMeta = serde_json::from_value(value).unwrap();
        assert_eq!(restored, meta);
    }

    #[test]
    fn session_meta_roundtrips_draft_images() {
        let meta = SessionMeta {
            input_draft_images: vec![StoredImage {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            }],
            ..SessionMeta::default()
        };

        let restored: SessionMeta =
            serde_json::from_value(serde_json::to_value(&meta).unwrap()).unwrap();

        assert_eq!(restored, meta);
    }

    #[test]
    fn session_meta_roundtrips_active_head_and_pending_revert() {
        let original_head = CaudraId::generate();
        let target_head = CaudraId::generate();
        let meta = SessionMeta {
            history_head: Some(target_head),
            pending_revert: Some(PendingConversationRevert {
                original_head: Some(original_head),
                target_head: Some(target_head),
                original_workspace_head: Some(Some(original_head).into()),
                workspace_head: Some(Some(target_head).into()),
                file_status: None,
                restore_operation: Some(super::PendingRestoreOperation {
                    id: CaudraId::generate(),
                    kind: super::PendingRestoreKind::Revert,
                    phase: super::PendingRestorePhase::Intent,
                    target_workspace_head: Some(target_head).into(),
                    conversation_target: Some(Some(target_head).into()),
                    overwrite: true,
                }),
            }),
            ..SessionMeta::default()
        };

        let restored: SessionMeta =
            serde_json::from_value(serde_json::to_value(&meta).unwrap()).unwrap();

        assert_eq!(restored, meta);
    }

    impl TitleSource for Value {
        fn first_user_text(&self) -> Option<&str> {
            if self.get("role")?.as_str()? != "user" {
                return None;
            }
            self.get("content")?.as_array()?.iter().find_map(|b| {
                if b.get("type")?.as_str()? == "text" {
                    let text = b.get("text")?.as_str()?;
                    (!text.is_empty()).then_some(text)
                } else {
                    None
                }
            })
        }
    }

    fn user_message(text: &str) -> Value {
        text_message("user", text)
    }

    fn assistant_message(text: &str) -> Value {
        text_message("assistant", text)
    }

    fn text_message(role: &str, text: &str) -> Value {
        serde_json::json!({
            "role": role,
            "content": [{"type": "text", "text": text}]
        })
    }

    fn write_legacy_jsonl(path: &Path, session: &TestSession) {
        let mut file = std::fs::File::create(path).unwrap();
        write_full_session(&mut file, session).unwrap();
    }

    #[test]
    fn prune_orphans_drops_unreachable_tool_state() {
        fn ids(m: &Value) -> Vec<String> {
            vec![m.as_str().unwrap().to_owned()]
        }
        fn subagent(id: &str) -> StoredSubagent {
            StoredSubagent {
                tool_use_id: id.into(),
                parent_tool_use_id: None,
                root_tool_use_id: None,
                name: "sub".into(),
                model: None,
                thinking: None,
                fast: false,
                outcome: StoredSubagentOutcome::Unknown,
            }
        }

        let mut session: TestSession = Session::new("model", "/p");
        session.push_message("task-live".into());
        session
            .subagent_messages
            .insert("task-live".into(), Arc::new(vec!["sub-tool".into()]));
        session
            .subagent_messages
            .insert("task-stale".into(), Arc::new(vec!["stale-sub-tool".into()]));
        for id in ["task-live", "task-stale", "task-without-history"] {
            session
                .subagent_task_specs
                .insert(id.into(), subagent_spec());
        }
        session.set_subagents(vec![subagent("task-live"), subagent("task-stale")]);
        for id in ["task-live", "sub-tool", "stale-sub-tool", "orphan"] {
            session.insert_tool_output(id.into(), Value::Null);
        }

        session.prune_orphans(ids);

        assert_eq!(
            session.subagent_messages().keys().collect::<Vec<_>>(),
            ["task-live"]
        );
        let subagent_ids: Vec<_> = session
            .subagents()
            .iter()
            .map(|sa| sa.tool_use_id.as_str())
            .collect();
        assert_eq!(subagent_ids, ["task-live"]);
        assert_eq!(
            session.subagent_task_specs().keys().collect::<Vec<_>>(),
            ["task-live"]
        );
        let mut outputs: Vec<_> = session.tool_outputs().keys().cloned().collect();
        outputs.sort();
        assert_eq!(outputs, ["sub-tool", "task-live"]);
    }

    #[test]
    fn roundtrip_save_load() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession =
            Session::new("anthropic/claude-sonnet-4", "/home/test/project");
        session.push_message(user_message("hello"));
        session.set_subagent_history(
            "tool-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
            Some(subagent_spec()),
        );
        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.model, "anthropic/claude-sonnet-4");
        assert_eq!(loaded.cwd, "/home/test/project");
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.version, SESSION_VERSION);
        assert_eq!(loaded.subagent_messages["tool-1"].len(), 2);
        assert_eq!(loaded.subagent_task_specs()["tool-1"], subagent_spec());
    }

    #[test]
    fn roundtrip_usage_by_model() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("anthropic/claude-sonnet-4", "/project");
        session.add_model_usage(
            "claude-sonnet-4",
            StoredTokenUsage {
                input: 100,
                output: 20,
                cache_creation: 5,
                cache_read: 40,
                cost: Some(SONNET_COST),
                subscription_cost: None,
            },
        );
        session.add_model_usage(
            "claude-haiku-4",
            StoredTokenUsage {
                input: 30,
                output: 10,
                cost: Some(HAIKU_COST),
                ..Default::default()
            },
        );
        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        let sonnet = &loaded.usage_by_model()["claude-sonnet-4"];
        assert_eq!(sonnet.input, 100);
        assert_eq!(sonnet.output, 20);
        assert_eq!(sonnet.cache_read, 40);
        assert_eq!(sonnet.total_input(), 145);
        assert_eq!(sonnet.cost, Some(SONNET_COST));
        assert_eq!(loaded.usage_by_model()["claude-haiku-4"].total(), 40);
        assert_eq!(
            loaded.usage_by_model()["claude-haiku-4"].cost,
            Some(HAIKU_COST)
        );
    }

    /// A turn that reports no price must not erase what was already billed.
    #[test_case(None, None, None ; "unpriced_stays_unpriced")]
    #[test_case(None, Some(SONNET_COST), Some(SONNET_COST) ; "first_price_starts_the_total")]
    #[test_case(Some(SONNET_COST), Some(HAIKU_COST), Some(SONNET_COST + HAIKU_COST) ; "priced_turns_accumulate")]
    #[test_case(Some(SONNET_COST), None, Some(SONNET_COST) ; "unpriced_turn_keeps_the_total")]
    fn add_cost_only_grows_a_total(
        mut total: Option<f64>,
        addend: Option<f64>,
        expected: Option<f64>,
    ) {
        super::add_cost(&mut total, addend);
        assert_eq!(total, expected);
    }

    fn usage(input: u32, cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input,
            cost,
            ..Default::default()
        }
    }

    /// A cache write is a token that had to be sent, so it counts against the
    /// rate; a provider that reports no prompt tokens at all reports no rate.
    #[test_case(0, 0, 0, None ; "nothing_cacheable_has_no_rate")]
    #[test_case(0, 0, 100, Some(1.0) ; "wholly_cached_is_a_full_hit")]
    #[test_case(100, 0, 0, Some(0.0) ; "uncached_input_is_a_full_miss")]
    #[test_case(0, 100, 100, Some(0.5) ; "a_cache_write_counts_as_a_miss")]
    #[test_case(50, 50, 100, Some(0.5) ; "hits_over_every_prompt_token")]
    fn cache_hit_rate_scores_prompt_tokens(
        input: u32,
        cache_creation: u32,
        cache_read: u32,
        expected: Option<f64>,
    ) {
        let usage = StoredTokenUsage {
            input,
            cache_creation,
            cache_read,
            output: 999,
            ..Default::default()
        };
        assert_eq!(usage.cache_hit_rate(), expected);
    }

    /// An entry that never reported a price has to come back unpriced rather
    /// than free, or the next priced turn starts a total that claims every
    /// earlier turn cost nothing.
    #[test]
    fn unpriced_usage_entry_stays_unpriced_across_a_reload() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.add_model_usage("m", usage(7, None));
        session.save(&state_dir).unwrap();

        let entry = TestSession::load(session.id, &state_dir)
            .unwrap()
            .usage_by_model()["m"];

        assert_eq!(entry.cost, None, "no price means unpriced, not free");
        assert_eq!(entry.input, 7);
    }

    /// What a turn billed is written verbatim, read back verbatim, and keeps
    /// adding up after a reload. A later unpriced turn must not throw away what
    /// the earlier ones paid.
    #[test]
    fn recorded_costs_survive_a_reload_and_keep_adding_up() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("anthropic/claude-sonnet-4", "/project");
        session.add_model_usage("claude-sonnet-4", usage(100, Some(SONNET_COST)));
        session.add_model_usage("claude-haiku-4", usage(30, None));
        session.save(&state_dir).unwrap();

        let mut loaded = TestSession::load(session.id, &state_dir).unwrap();
        loaded.add_model_usage("claude-sonnet-4", usage(50, None));
        loaded.add_model_usage("claude-haiku-4", usage(10, Some(HAIKU_COST)));

        let sonnet = loaded.usage_by_model()["claude-sonnet-4"];
        assert_eq!((sonnet.input, sonnet.cost), (150, Some(SONNET_COST)));
        let haiku = loaded.usage_by_model()["claude-haiku-4"];
        assert_eq!((haiku.input, haiku.cost), (40, Some(HAIKU_COST)));
    }

    /// `subagents` and `usage_by_model` moved off `SessionMeta` onto the
    /// session, which must not move them in the archived meta record: they were
    /// flattened in beside the meta fields and they still sit there.
    #[test]
    fn session_owned_fields_keep_their_place_in_the_meta_record() {
        let mut session: TestSession = Session::new("m", "/project");
        session.set_subagents(vec![StoredSubagent {
            tool_use_id: "t1".into(),
            parent_tool_use_id: None,
            root_tool_use_id: None,
            name: "child".into(),
            model: None,
            thinking: None,
            fast: false,
            outcome: StoredSubagentOutcome::Done,
        }]);
        session.add_model_usage("m", usage(7, None));
        session.meta.fast = true;

        let record: Value = serde_json::from_slice(&meta_record(&session).unwrap()).unwrap();

        assert_eq!(record["t"], META_RECORD);
        assert_eq!(record["subagents"][0]["name"], "child");
        assert_eq!(record["usage_by_model"]["m"]["input"], 7);
        assert_eq!(record["fast"], true);
    }

    #[test]
    fn empty_subagent_history_roundtrips_with_its_spec() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.set_subagent_history("sub-1".into(), Vec::new(), Some(subagent_spec()));

        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        assert!(loaded.subagent_messages()["sub-1"].is_empty());
        assert_eq!(loaded.subagent_task_specs()["sub-1"], subagent_spec());
    }

    #[test]
    fn generic_subagent_kind_roundtrips() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.set_subagent_history(
            "generic-1".into(),
            vec![user_message("prompt")],
            Some(StoredSubagentTaskSpec::generic()),
        );

        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        assert!(loaded.subagent_task_specs()["generic-1"].is_generic());
    }

    #[test]
    fn subagent_version_kind_roundtrips() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.set_subagent_history(
            "continuation-call".into(),
            vec![user_message("continued")],
            Some(StoredSubagentTaskSpec::version()),
        );

        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        assert!(loaded.subagent_task_specs()["continuation-call"].is_version());
    }

    /// The mirror re-adopts the run's snapshot on every checkpoint; that must
    /// not erase the void minted by a same-frame in-place replacement, or the
    /// writer appends onto a stale prefix and persists a mixed transcript.
    #[test]
    fn snapshot_adoption_does_not_erase_a_local_rewrite() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session: Arc<TestSession> = Arc::new(Session::new("m", "/project"));
        let run = HistorySnapshot {
            epoch: next_epoch(),
            messages: Arc::new(vec![user_message("hi")]),
        };
        let meta = session.meta.clone();
        Session::checkpoint(&mut session, Some(&run), meta.clone(), Value::Null);
        Arc::make_mut(&mut session)
            .set_subagent_messages("sub-1".into(), vec![user_message("old")]);
        let mut cursor = None;
        write_through(&mut database, &mut cursor, &session);

        Arc::make_mut(&mut session)
            .set_subagent_messages("sub-1".into(), vec![user_message("new")]);
        let advanced = HistorySnapshot {
            epoch: run.epoch,
            messages: Arc::new(vec![user_message("hi"), assistant_message("reply")]),
        };
        Session::checkpoint(&mut session, Some(&advanced), meta, Value::Null);
        write_through(&mut database, &mut cursor, &session);

        assert_same_session(&load_from(&database, session.id), &session);
    }

    #[test]
    fn replacing_subagent_messages_rewrites_the_stored_history() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        let mut cursor = None;
        write_through(&mut database, &mut cursor, &session);

        session.set_subagent_messages("sub-1".into(), vec![user_message("old")]);
        write_through(&mut database, &mut cursor, &session);

        session.set_subagent_messages("sub-1".into(), vec![user_message("new")]);
        write_through(&mut database, &mut cursor, &session);

        assert_same_session(&load_from(&database, session.id), &session);
    }

    #[test]
    fn cwd_and_model_changes_survive_reload() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/old");
        session.push_message(user_message("hi"));
        session.save(&state_dir).unwrap();

        session.set_model("m2".into());
        session.set_cwd("/new".into());
        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        assert_eq!(loaded.model, "m2");
        assert_eq!(loaded.cwd, "/new");
    }

    #[test]
    fn rewind_compact() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        for i in 0..10 {
            session.push_message(user_message(&format!("msg-{i}")));
        }
        session.set_subagent_messages(
            "sub-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
        );
        session.save(&state_dir).unwrap();

        session.truncate_messages(5);
        session.tool_outputs.clear();
        session.subagent_messages.remove("sub-1");
        session.save(&state_dir).unwrap();

        session.push_message(user_message("after-compact-1"));
        session.push_message(user_message("after-compact-2"));
        session.push_message(user_message("after-compact-3"));
        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        assert_eq!(loaded.messages().len(), 8);
        assert!(loaded.subagent_messages().is_empty());
    }

    fn archive_dir_for(dir: &StateDir, id: CaudraId) -> PathBuf {
        dir.path()
            .join(SESSIONS_DIR)
            .join(ARCHIVE_DIR)
            .join(id.to_string())
    }

    fn archive_paths(dir: &StateDir, id: CaudraId) -> Vec<PathBuf> {
        let mut paths = fs::read_dir(archive_dir_for(dir, id))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn archive_records(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn archived_messages(path: &Path) -> Vec<Value> {
        archive_records(path)
            .into_iter()
            .filter(|record| record["t"] == MSG_RECORD)
            .map(|record| record["d"].clone())
            .collect()
    }

    #[test]
    fn rewrite_dropping_messages_archives_the_old_file() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("model", "/p");
        for i in 0..5 {
            session.push_message(user_message(&format!("turn {i}")));
        }
        session.save(&state_dir).unwrap();

        session.replace_messages(vec![user_message("summary")]);
        session.save(&state_dir).unwrap();

        let live = TestSession::load(session.id, &state_dir).unwrap();
        assert_eq!(live.messages().len(), 1);
        let archives = archive_paths(&state_dir, session.id);
        assert_eq!(archives.len(), 1);
        assert_eq!(archived_messages(&archives[0]).len(), 5);
    }

    #[test]
    fn rewrite_without_shrink_does_not_archive() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.save(&state_dir).unwrap();

        session.push_message(user_message("two"));
        session.save(&state_dir).unwrap();

        assert!(!archive_dir_for(&state_dir, session.id).exists());
    }

    /// The archive is the only copy of the dropped turns, so it has to be a
    /// complete session log in the current format, not just the messages.
    #[test]
    fn archived_file_round_trips() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("model", "/p");
        let pre: Vec<Value> = (0..3).map(|i| user_message(&format!("turn {i}"))).collect();
        for msg in &pre {
            session.push_message(msg.clone());
        }
        session.save(&state_dir).unwrap();

        session.replace_messages(vec![assistant_message("summary")]);
        session.save(&state_dir).unwrap();

        let archives = archive_paths(&state_dir, session.id);
        assert_eq!(archives.len(), 1);
        let records = archive_records(&archives[0]);
        assert_eq!(records[0]["t"], HEADER_RECORD);
        assert_eq!(records[0]["v"], LOG_FORMAT_VERSION);
        assert_eq!(records[0]["id"], session.id.to_string());
        assert_eq!(records.last().unwrap()["t"], META_RECORD);
        assert_eq!(archived_messages(&archives[0]), pre);
    }

    #[test]
    fn archive_retention_keeps_newest_three() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("seed"));
        session.save(&state_dir).unwrap();
        for round in 1..=5 {
            for _ in 0..round {
                session.push_message(user_message(&format!("turn {round}")));
            }
            session.save(&state_dir).unwrap();
            session.replace_messages(vec![user_message(&format!("summary {round}"))]);
            session.save(&state_dir).unwrap();
        }

        let archives = archive_paths(&state_dir, session.id);
        assert_eq!(archives.len(), ARCHIVE_KEEP);
        let mut msg_counts: Vec<usize> = archives
            .iter()
            .map(|path| archived_messages(path).len())
            .collect();
        msg_counts.sort_unstable();
        assert_eq!(msg_counts, [4, 5, 6]);
    }

    /// A new name has to beat every name already there, or pruning would read
    /// the fresh archive as the oldest and eat it.
    #[test]
    fn archive_names_count_up_from_the_newest() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.push_message(user_message("two"));
        session.save(&state_dir).unwrap();

        let archive_dir = archive_dir_for(&state_dir, session.id);
        fs::create_dir_all(&archive_dir).unwrap();
        let existing = archive_dir.join(format!("{EXISTING_ARCHIVE_SEQ}.jsonl"));
        fs::write(&existing, "").unwrap();

        session.replace_messages(vec![user_message("summary")]);
        session.save(&state_dir).unwrap();

        let fresh = archive_dir.join(format!("{}.jsonl", EXISTING_ARCHIVE_SEQ + 1));
        assert_eq!(
            archive_paths(&state_dir, session.id),
            vec![existing, fresh.clone()]
        );
        assert_eq!(archived_messages(&fresh).len(), 2);
    }

    #[test]
    fn archive_retention_honors_the_byte_budget() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.push_message(user_message("two"));
        session.save(&state_dir).unwrap();

        let archive_dir = archive_dir_for(&state_dir, session.id);
        fs::create_dir_all(&archive_dir).unwrap();
        let fakes: Vec<PathBuf> = (1..=3)
            .map(|seq| {
                let path = archive_dir.join(format!("{seq}.jsonl"));
                // Sparse: the length is all the budget looks at.
                fs::File::create(&path)
                    .unwrap()
                    .set_len(FAKE_ARCHIVE_BYTES)
                    .unwrap();
                path
            })
            .collect();

        session.replace_messages(vec![user_message("summary")]);
        session.save(&state_dir).unwrap();

        let archives = archive_paths(&state_dir, session.id);
        assert_eq!(archives.len(), 2);
        assert!(archives.contains(&fakes[2]));
        assert!(!fakes[0].exists());
        assert!(!fakes[1].exists());
    }

    #[test]
    fn delete_removes_archive_dir() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.push_message(user_message("two"));
        session.save(&state_dir).unwrap();
        session.replace_messages(vec![user_message("summary")]);
        session.save(&state_dir).unwrap();
        let archive_dir = archive_dir_for(&state_dir, session.id);
        assert!(archive_dir.exists());

        TestSession::delete(session.id, &state_dir).unwrap();

        assert!(!archive_dir.exists());
        assert!(matches!(
            TestSession::load(session.id, &state_dir),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
    }

    #[test]
    fn load_nonexistent_returns_not_found() {
        let (_temp, state_dir) = state_dir();
        let err = TestSession::load(CaudraId::generate(), &state_dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn list_filters_by_cwd() {
        let (_temp, state_dir) = state_dir();
        let mut s1: TestSession = Session::new("m", "/project-a");
        let mut s2: TestSession = Session::new("m", "/project-b");
        let mut s3: TestSession = Session::new("m", "/project-a");
        s1.save(&state_dir).unwrap();
        s2.save(&state_dir).unwrap();
        s3.save(&state_dir).unwrap();

        let list = TestSession::list("/project-a", &state_dir).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|s| s.id != s2.id));
    }

    /// `Session::save` stamps the current time, so ordering is only observable
    /// through the database, which takes the timestamp the session carries.
    fn save_with_time(session: &mut TestSession, dir: &StateDir, time: u64) {
        session.updated_at = time;
        SessionDatabase::open(dir)
            .unwrap()
            .save(session, None)
            .unwrap();
    }

    #[test]
    fn latest_returns_most_recent_for_cwd() {
        let (_temp, state_dir) = state_dir();
        let mut s1: TestSession = Session::new("m", "/project");
        s1.title = "first".into();
        save_with_time(&mut s1, &state_dir, 1000);

        let mut s2: TestSession = Session::new("m", "/other");
        save_with_time(&mut s2, &state_dir, 2000);

        let mut s3: TestSession = Session::new("m", "/project");
        s3.title = "latest".into();
        save_with_time(&mut s3, &state_dir, 3000);

        let latest = TestSession::latest("/project", &state_dir)
            .unwrap()
            .unwrap();
        assert_eq!(latest.title, "latest");
    }

    #[test]
    fn latest_follows_a_session_to_its_new_cwd() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/old");
        session.save(&state_dir).unwrap();

        session.set_cwd("/new".into());
        session.save(&state_dir).unwrap();

        assert!(TestSession::latest("/old", &state_dir).unwrap().is_none());
        assert_eq!(
            TestSession::latest("/new", &state_dir).unwrap().unwrap().id,
            session.id
        );
    }

    #[test_case("short title", "short title" ; "short_passthrough")]
    #[test_case("", DEFAULT_TITLE ; "empty_defaults")]
    #[test_case(LONG_TITLE, LONG_TITLE_TRUNCATED ; "long_truncates_at_word")]
    #[test_case("one\n\ntwo\t three", "one two three" ; "whitespace_collapses")]
    fn title_extraction(input: &str, expected: &str) {
        let messages: Vec<Value> = if input.is_empty() {
            vec![]
        } else {
            vec![user_message(input)]
        };
        assert_eq!(generate_title(&messages), expected);
    }

    #[test_case(DEFAULT_TITLE, MODEL_TITLE ; "replaces_default")]
    #[test_case(TITLE_PROMPT, MODEL_TITLE ; "replaces_heuristic")]
    #[test_case(RENAMED_TITLE, RENAMED_TITLE ; "keeps_manual_rename")]
    #[test_case(FORK_TITLE, FORK_TITLE ; "keeps_fork_title")]
    fn set_title_if_auto_only_replaces_derived_titles(current: &str, expected: &str) {
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message(TITLE_PROMPT));
        session.set_title(current.into());

        session.set_title_if_auto(MODEL_TITLE.into());

        assert_eq!(session.title, expected);
    }

    #[test]
    fn delete_removes_only_the_named_session() {
        let (_temp, state_dir) = state_dir();
        let mut doomed: TestSession = Session::new("m", "/project");
        doomed.save(&state_dir).unwrap();
        let mut kept: TestSession = Session::new("m", "/other");
        kept.save(&state_dir).unwrap();

        TestSession::delete(doomed.id, &state_dir).unwrap();

        assert!(matches!(
            TestSession::load(doomed.id, &state_dir),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
        assert_eq!(
            TestSession::latest("/other", &state_dir)
                .unwrap()
                .unwrap()
                .id,
            kept.id
        );
    }

    #[test]
    fn public_delete_removes_managed_outputs() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.save(&state_dir).unwrap();
        let store = ToolOutputStore::new(state_dir.clone());
        let output = store.put(session.id, "managed output").unwrap();

        TestSession::delete(session.id, &state_dir).unwrap();

        assert!(matches!(
            store.read(session.id, output.id, 1, 1),
            Err(ToolOutputError::NotFound { .. })
        ));
    }

    #[test]
    fn public_delete_succeeds_without_managed_output_directory() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.save(&state_dir).unwrap();

        TestSession::delete(session.id, &state_dir).unwrap();

        assert!(!state_dir.path().join("tool-output").exists());
    }

    #[test]
    fn public_delete_retry_cleans_outputs_after_session_is_already_gone() {
        let (_temp, state_dir) = state_dir();
        let session_id = CaudraId::generate();
        let store = ToolOutputStore::new(state_dir.clone());
        let output = store.put(session_id, "leftover output").unwrap();

        let error = TestSession::delete(session_id, &state_dir).unwrap_err();

        assert!(matches!(
            error,
            SessionError::Storage(StorageError::NotFound(_))
        ));
        assert!(matches!(
            store.read(session_id, output.id, 1, 1),
            Err(ToolOutputError::NotFound { .. })
        ));
    }

    #[test]
    fn public_delete_retry_cleans_outputs_after_database_row_is_gone() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.save(&state_dir).unwrap();
        SessionDatabase::open(&state_dir)
            .unwrap()
            .delete(session.id, None)
            .unwrap();
        let store = ToolOutputStore::new(state_dir.clone());
        let output = store.put(session.id, "leftover output").unwrap();

        let error = TestSession::delete(session.id, &state_dir).unwrap_err();

        assert!(matches!(
            error,
            SessionError::Storage(StorageError::NotFound(_))
        ));
        assert!(matches!(
            store.read(session.id, output.id, 1, 1),
            Err(ToolOutputError::NotFound { .. })
        ));
    }

    #[test]
    fn state_dir_session_ids_ignore_jsonl_files() {
        let (_temp, state_dir) = state_dir();
        let sessions_dir = state_dir.ensure_subdir(SESSIONS_DIR).unwrap();
        let mut canonical: TestSession = Session::new("m", "/canonical");
        canonical.save(&state_dir).unwrap();
        let stray: TestSession = Session::new("m", "/stray");
        write_legacy_jsonl(&sessions_dir.join(format!("{}.jsonl", stray.id)), &stray);

        let ids = persisted_session_ids(&state_dir).unwrap();

        assert_eq!(ids, vec![canonical.id]);
    }

    #[test]
    fn delete_nonexistent_returns_not_found() {
        let (_temp, state_dir) = state_dir();
        let err = TestSession::delete(CaudraId::generate(), &state_dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn title_unicode_safe() {
        let input = "あ".repeat(100);
        let title = generate_title(&[user_message(&input)]);
        assert!(title.len() <= MAX_TITLE_LEN * 4);
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn session_meta_backward_compat_defaults() {
        let json = r#"{"mode":"build"}"#;
        let meta: SessionMeta = serde_json::from_str(json).unwrap();
        assert!(meta.thinking.is_none());
        assert!(!meta.fast);
        assert!(!meta.queued_messages_together);
        assert!(meta.queued_message_admissions.is_empty());
        assert!(meta.unsent_subagent_messages.is_empty());
        assert!(meta.yolo.is_none());
    }

    /// Sessions saved before the legacy flag was removed still carry its key.
    #[test_case(r#"{"workflow":true}"# ; "workflow_key")]
    #[test_case(r#"{"orchestration":true}"# ; "orchestration_key")]
    fn session_meta_ignores_removed_flag_keys(json: &str) {
        let meta: SessionMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta, SessionMeta::default());
    }

    #[test]
    fn session_meta_rejects_string_queued_messages() {
        let json = r#"{"queued_messages":["legacy"]}"#;
        assert!(serde_json::from_str::<SessionMeta>(json).is_err());
    }

    /// Storing a mode nobody picked would make every front end inherit this
    /// crate's guess instead of its own default.
    #[test]
    fn a_new_session_has_not_chosen_a_mode() {
        let session: TestSession = Session::new("m", "/project");
        assert_eq!(session.meta.mode, None, "{MODE_UNCHOSEN}");
    }

    #[test]
    fn session_meta_persists_through_save_load() {
        let (_temp, state_dir) = state_dir();
        let mut session: TestSession = Session::new("m", "/project");
        session.meta.thinking = Some(StoredThinking::Budget { tokens: 8192 });
        session.meta.fast = true;
        session.meta.queued_messages_together = true;
        session.meta.queued_messages = vec![
            StoredQueuedPrompt {
                text: "guide".into(),
                images: vec![StoredImage {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
                paste_ranges: vec![StoredPasteRange { start: 0, end: 5 }],
            },
            StoredQueuedPrompt {
                text: "next".into(),
                images: Vec::new(),
                paste_ranges: Vec::new(),
            },
        ];
        session.meta.queued_message_admissions =
            vec![StoredPromptAdmission::Steer, StoredPromptAdmission::Queue];
        session.meta.unsent_subagent_messages.insert(
            "task-1".into(),
            vec![StoredQueuedDraft {
                text: "follow up".into(),
                paste_ranges: vec![StoredPasteRange { start: 0, end: 9 }],
            }],
        );
        session.meta.yolo = Some(true);
        session.save(&state_dir).unwrap();

        let loaded = TestSession::load(session.id, &state_dir).unwrap();
        assert_eq!(
            loaded.meta.thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );
        assert!(loaded.meta.fast);
        assert!(loaded.meta.queued_messages_together);
        assert_eq!(loaded.meta.queued_messages, session.meta.queued_messages);
        assert_eq!(
            loaded.meta.queued_message_admissions,
            [StoredPromptAdmission::Steer, StoredPromptAdmission::Queue]
        );
        assert_eq!(
            loaded.meta.unsent_subagent_messages["task-1"][0].text,
            "follow up"
        );
        assert_eq!(loaded.meta.yolo, Some(true));
    }

    // -- The writer never guesses --

    const PROPERTY_SEED: u64 = 0x2545_F491_4F6C_DD1D;
    const PROPERTY_STEPS: usize = 500;
    const MUTATION_KINDS: u64 = 8;

    /// Deterministic xorshift so a failure is always the same failure.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn tool_message(id: &str) -> Value {
        serde_json::json!({ "role": "assistant", "tool": id })
    }

    fn tool_ids(m: &Value) -> Vec<String> {
        m.get("tool")
            .and_then(Value::as_str)
            .map(|s| vec![s.to_owned()])
            .unwrap_or_default()
    }

    /// What the storage writer does: hand back the cursor of the last write so
    /// the database can tell an append from a replacement instead of guessing
    /// from the shape of the session.
    fn write_through(
        database: &mut SessionDatabase,
        cursor: &mut Option<SessionCursor>,
        session: &TestSession,
    ) {
        *cursor = Some(database.save(session, cursor.as_ref()).unwrap());
    }

    fn load_from(database: &SessionDatabase, id: CaudraId) -> TestSession {
        database.load(id).unwrap()
    }

    #[track_caller]
    fn assert_same_session(loaded: &TestSession, expected: &TestSession) {
        assert_eq!(loaded.messages(), expected.messages(), "messages");
        assert_eq!(loaded.tool_outputs(), expected.tool_outputs(), "outputs");
        assert_eq!(
            loaded.subagent_messages(),
            expected.subagent_messages(),
            "subagent messages",
        );
        assert_eq!(
            loaded.subagent_task_specs(),
            expected.subagent_task_specs(),
            "subagent task specs",
        );
        assert_eq!(loaded.title, expected.title, "title");
        assert_eq!(loaded.meta, expected.meta, "meta");
        assert_eq!(loaded.updated_at, expected.updated_at, "updated_at");
    }

    fn mutate(session: &mut TestSession, rng: &mut Rng, step: usize) {
        let slot = format!("t{}", rng.below(4));
        match rng.below(MUTATION_KINDS) {
            0 => session.push_message(user_message(&format!("msg-{step}"))),
            1 => {
                session.push_message(tool_message(&slot));
                session.push_message(assistant_message("reply"));
            }
            2 => session.insert_tool_output(slot, Value::from(format!("out-{step}"))),
            3 => {
                let len = rng.below(4) as usize;
                let msgs = (0..len)
                    .map(|i| user_message(&format!("sub-{i}")))
                    .collect();
                session.set_subagent_messages(slot, msgs);
            }
            4 => {
                let len = session.messages().len();
                session.truncate_messages(len.saturating_sub(1 + rng.below(3) as usize));
            }
            5 => session.replace_messages(vec![user_message(&format!("fresh-{step}"))]),
            6 => session.prune_orphans(tool_ids),
            _ => {
                session.set_title(format!("title-{step}"));
                session.set_meta(SessionMeta {
                    input_draft: Some(format!("draft-{step}")),
                    ..session.meta.clone()
                });
            }
        }
    }

    /// Every mutation kind in random order, with snapshots dropped here and
    /// there like the writer coalescing them. Whatever the script, reloading
    /// must give back the live session.
    #[test]
    fn random_mutation_script_round_trips_through_storage() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        let mut cursor = None;
        write_through(&mut database, &mut cursor, &session);
        let mut rng = Rng(PROPERTY_SEED);

        for step in 0..PROPERTY_STEPS {
            mutate(&mut session, &mut rng, step);
            // Dropping a snapshot is what coalescing does, and the next write
            // must still land on a store that matches.
            if rng.below(3) == 0 {
                continue;
            }
            write_through(&mut database, &mut cursor, &session);
        }
        write_through(&mut database, &mut cursor, &session);

        assert_same_session(&load_from(&database, session.id), &session);
    }

    /// `Arc::make_mut` deep-copies the session while the writer holds the last
    /// snapshot, and a checkpoint that changes nothing must not pay for it.
    #[test]
    fn unchanged_checkpoint_does_not_clone_the_session() {
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("hello"));
        let snapshot = HistorySnapshot::new(session.messages().to_vec());
        let mut session = Arc::new(session);
        let meta = session.meta.clone();
        Session::checkpoint(&mut session, Some(&snapshot), meta.clone(), Value::Null);

        let held = Arc::clone(&session);
        Session::checkpoint(&mut session, Some(&snapshot), meta.clone(), Value::Null);
        assert!(Arc::ptr_eq(&held, &session), "no change, no clone");

        Session::checkpoint(
            &mut session,
            Some(&snapshot),
            SessionMeta {
                input_draft: Some("draft".into()),
                ..meta
            },
            Value::Null,
        );
        assert!(!Arc::ptr_eq(&held, &session));
        assert_eq!(session.meta.input_draft.as_deref(), Some("draft"));
        assert!(session.revision() > held.revision());
    }

    /// What the owner types sits in `meta`, so `content_revision` is what tells
    /// a keystroke, which can wait for the ones behind it, from a tool result,
    /// which has to be on disk before the next crash.
    #[test]
    fn a_meta_only_change_leaves_content_revision_alone() {
        let mut session: TestSession = Session::new("m", "/project");
        let (revision, content) = (session.revision(), session.content_revision());

        session.set_meta(SessionMeta {
            input_draft: Some(PENDING_DRAFT.into()),
            ..session.meta.clone()
        });
        assert!(session.revision() > revision, "still needs writing");
        assert_eq!(session.content_revision(), content, "but it can wait");

        session.push_message(user_message("hello"));
        assert!(session.content_revision() > content);
    }

    /// A mutator called with the value already there is not a change, and a
    /// truncate that cuts nothing must leave the epoch alone or every open
    /// cursor into the store dies for nothing.
    #[test]
    fn no_op_mutators_leave_the_session_and_its_cursors_alone() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("a"));
        session.push_message(assistant_message("b"));
        let mut cursor = None;
        write_through(&mut database, &mut cursor, &session);
        let (revision, updated_at, epoch) = (session.revision(), session.updated_at, session.epoch);

        session.set_title(session.title.clone());
        session.set_meta(session.meta.clone());
        session.truncate_messages(session.messages().len());
        session.truncate_messages(session.messages().len() + 1);

        assert_eq!(session.revision(), revision);
        assert_eq!(session.updated_at, updated_at);
        assert_eq!(session.epoch, epoch);

        session.push_message(user_message("c"));
        write_through(&mut database, &mut cursor, &session);
        assert_same_session(&load_from(&database, session.id), &session);
    }

    /// The corruption the epoch exists for. A rewind mints a new run, so a
    /// snapshot still in flight carries the pre-rewind messages: longer than
    /// what was rewritten, yet sharing only its head. Going by length alone
    /// would splice its tail on and leave storage holding `[a, d, c]` while the
    /// session holds `[a, b, c]`.
    #[test]
    fn stale_snapshot_after_a_rewind_is_rewritten_not_spliced() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let produced = HistorySnapshot::new(vec![
            user_message("a"),
            assistant_message("b"),
            user_message("c"),
        ]);
        let mut session: Arc<TestSession> = Arc::new(Session::new("m", "/project"));
        let meta = session.meta.clone();
        Session::checkpoint(&mut session, Some(&produced), meta.clone(), Value::Null);
        let mut cursor = None;
        write_through(&mut database, &mut cursor, &session);

        let live = Arc::make_mut(&mut session);
        live.truncate_messages(1);
        live.push_message(user_message("d"));
        write_through(&mut database, &mut cursor, &session);

        Session::checkpoint(&mut session, Some(&produced), meta, Value::Null);
        write_through(&mut database, &mut cursor, &session);

        assert_same_session(&load_from(&database, session.id), &session);
    }
}
