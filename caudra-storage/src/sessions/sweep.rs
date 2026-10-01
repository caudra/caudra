//! Plans and executes retention over the session repository. The CLI and
//! the background sweep share this code so a dry run shows exactly what the
//! sweep would do.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use caudra_workspace::RecordHolder;
use jiff::Zoned;
use serde::Serialize;
use tracing::{info, warn};

use super::change_stores::ChangeStores;
use super::database::{WORKSPACE_CHANGES_DIR, directory_bytes, history_uuid_timestamp};
use super::lease::SessionLease;
use super::progress::{PRUNE, PruneEvent};
use super::{
    ARCHIVE_DIR, CheckpointResult, SESSIONS_DIR, SessionDatabase, SessionError, TrimReport,
};
use crate::id::CaudraId;
use crate::retention::{self, Decision, GroupBy, KeepPolicy, SessionFacts};
use crate::tool_outputs::ToolOutputStore;
use crate::{StateDir, StorageError, lock_session_artifacts, try_exclusive_state_lock};

const SWEEP_LOCK_FILE: &str = "caudra.sqlite.sweep.lock";
const LAST_SWEEP_KEY: &str = "retention.last_sweep_at";
const CLEANUP_JOBS_PHASE: &str = "completing cleanup jobs";
const ORPHAN_SCAN_PHASE: &str = "scanning orphaned artifacts";
const CHANGE_STORES_PHASE: &str = "cleaning change stores";
const TOOL_OUTPUT_PHASE: &str = "cleaning orphaned tool output";
const CHECKPOINT_PHASE: &str = "checkpointing the WAL";
const OWNER_FILE_MODE: u32 = 0o600;
/// Orphaned artifact directories younger than this may belong to a session
/// whose first save has not committed yet.
const ORPHAN_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The snapshot directories from before change records, which nothing reads.
pub(super) const LEGACY_SNAPSHOT_DIRS: [&str; 2] = ["session-snapshots", "workspace-snapshots"];
/// The file in a change store that every write to it rewrites.
const CHANGE_STORE_STATE_FILE: &str = "state";
const SKIP_OPEN: &str = "open in another Caudra instance";
const SKIP_PINNED: &str = "pinned";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Trim,
    Forget,
}

impl Action {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Trim => "trim",
            Self::Forget => "forget",
        }
    }

    pub fn past(self) -> &'static str {
        match self {
            Self::Trim => "trimmed",
            Self::Forget => "forgotten",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    PendingRevert,
    AlreadyTrimmed,
}

impl Skip {
    pub fn label(self) -> &'static str {
        match self {
            Self::PendingRevert => "pending revert",
            Self::AlreadyTrimmed => "already trimmed",
        }
    }
}

/// A session the policy dropped, with the artifact bytes the action frees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub session: SessionFacts,
    pub artifact_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub session: SessionFacts,
    pub reason: Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedGroup {
    pub key: String,
    pub keep: Vec<Decision>,
    pub act: Vec<Candidate>,
    pub skip: Vec<Skipped>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub action: Action,
    pub policy: KeepPolicy,
    pub group_by: GroupBy,
    pub directory: Option<String>,
    pub groups: Vec<PlannedGroup>,
}

impl Plan {
    pub fn candidates(&self) -> impl Iterator<Item = &Candidate> {
        self.groups.iter().flat_map(|group| group.act.iter())
    }

    pub fn candidate_count(&self) -> usize {
        self.groups.iter().map(|group| group.act.len()).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum OutcomeKind {
    Trimmed(TrimReport),
    Forgotten { artifact_bytes: u64 },
    Skipped { reason: &'static str },
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub session: SessionFacts,
    pub kind: OutcomeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExecuteReport {
    pub outcomes: Vec<Outcome>,
}

impl ExecuteReport {
    pub fn acted(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome.kind,
                    OutcomeKind::Trimmed(_) | OutcomeKind::Forgotten { .. }
                )
            })
            .count()
    }

    pub fn failed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome.kind, OutcomeKind::Failed { .. }))
            .count()
    }

    pub fn released_bytes(&self) -> u64 {
        self.outcomes
            .iter()
            .map(|outcome| match &outcome.kind {
                OutcomeKind::Trimmed(report) => {
                    report.artifact_bytes + report.tool_output_row_bytes
                }
                OutcomeKind::Forgotten { artifact_bytes } => *artifact_bytes,
                _ => 0,
            })
            .sum()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub cleanup_jobs_due: u64,
    pub cleanup_jobs_completed: u64,
    pub orphan_directories: u64,
    pub orphan_bytes: u64,
    pub orphan_tool_output_entries: u64,
    /// Change store holders with no session, released once past the grace
    /// period.
    pub orphan_record_holders: u64,
    /// What cleaning each change store down to its budget reclaimed.
    pub record_bytes_reclaimed: u64,
    /// Change stores that could not be pruned. The others still were.
    pub change_store_failures: u64,
    pub checkpoint: Option<CheckpointResult>,
    pub freelist_pages_before: u64,
    pub freelist_pages_after: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepPolicy {
    pub group_by: GroupBy,
    pub interval: Duration,
    pub trim: KeepPolicy,
    pub forget: KeepPolicy,
    /// The size each change store is cleaned down to.
    pub store_budget: NonZeroU64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SweepReport {
    pub trim: ExecuteReport,
    pub forget: ExecuteReport,
    pub prune: PruneReport,
}

/// Evaluates `policy` and decides what `action` would touch. Nothing is
/// modified. Sessions kept by the policy, and sessions the action must not
/// touch, are reported with their reasons.
pub fn plan(
    database: &SessionDatabase,
    action: Action,
    policy: KeepPolicy,
    group_by: GroupBy,
    directory: Option<&str>,
    now: &Zoned,
) -> Result<Plan, SessionError> {
    let facts = database.session_facts(directory)?;
    let groups = retention::apply(&policy, group_by, facts, now)
        .into_iter()
        .map(|group| {
            let mut planned = PlannedGroup {
                key: group.key,
                keep: Vec::new(),
                act: Vec::new(),
                skip: Vec::new(),
            };
            for decision in group.decisions {
                if decision.keep() {
                    planned.keep.push(decision);
                    continue;
                }
                let session = decision.session;
                let skip = if session.pending_revert {
                    Some(Skip::PendingRevert)
                } else if action == Action::Trim && session.is_trimmed() {
                    Some(Skip::AlreadyTrimmed)
                } else {
                    None
                };
                match skip {
                    Some(reason) => planned.skip.push(Skipped { session, reason }),
                    None => {
                        let artifact_bytes = database.artifact_bytes(session.id);
                        planned.act.push(Candidate {
                            session,
                            artifact_bytes,
                        });
                    }
                }
            }
            planned
        })
        .collect();
    Ok(Plan {
        action,
        policy,
        group_by,
        directory: directory.map(str::to_owned),
        groups,
    })
}

/// Applies a plan. Every candidate is leased first, so sessions open in this
/// or another process are skipped rather than raced.
pub fn execute(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    plan: &Plan,
) -> Result<ExecuteReport, SessionError> {
    let mut report = ExecuteReport::default();
    for candidate in plan.candidates() {
        let session = candidate.session.clone();
        let kind = act_on(database, state_dir, plan.action, candidate);
        match &kind {
            OutcomeKind::Trimmed(_) | OutcomeKind::Forgotten { .. } => {
                info!(session_id = %session.id, action = plan.action.verb(), "retention applied");
            }
            OutcomeKind::Skipped { reason } => {
                info!(session_id = %session.id, reason, "retention skipped session");
            }
            OutcomeKind::Failed { error } => {
                warn!(session_id = %session.id, error, "retention failed for session");
            }
        }
        report.outcomes.push(Outcome { session, kind });
    }
    if plan.action == Action::Forget && report.acted() > 0 {
        database.process_cleanup_jobs()?;
    }
    Ok(report)
}

fn act_on(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    action: Action,
    candidate: &Candidate,
) -> OutcomeKind {
    let id = candidate.session.id;
    let lease = match SessionLease::acquire(state_dir, id) {
        Ok(lease) => lease,
        Err(SessionError::SessionInUse { .. }) => {
            return OutcomeKind::Skipped { reason: SKIP_OPEN };
        }
        Err(error) => {
            return OutcomeKind::Failed {
                error: error.to_string(),
            };
        }
    };
    let result = match action {
        Action::Trim => database.trim(&lease).map(OutcomeKind::Trimmed),
        Action::Forget => database.delete(id, None).map(|_| OutcomeKind::Forgotten {
            artifact_bytes: candidate.artifact_bytes,
        }),
    };
    result.unwrap_or_else(|error| OutcomeKind::Failed {
        error: error.to_string(),
    })
}

/// Applies `action` to the sessions in `ids` regardless of policy. Pinned and
/// open sessions are refused, so naming a session directly still cannot
/// override the pin that protects it or race a process holding it.
pub fn apply_ids(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    action: Action,
    ids: &[CaudraId],
) -> Result<ExecuteReport, SessionError> {
    let facts = database.session_facts(None)?;
    let mut report = ExecuteReport::default();
    for id in ids {
        let Some(session) = facts.iter().find(|facts| facts.id == *id).cloned() else {
            return Err(StorageError::NotFound(id.to_string()).into());
        };
        let kind = if session.pinned {
            OutcomeKind::Skipped {
                reason: SKIP_PINNED,
            }
        } else {
            let candidate = Candidate {
                artifact_bytes: database.artifact_bytes(*id),
                session: session.clone(),
            };
            act_on(database, state_dir, action, &candidate)
        };
        report.outcomes.push(Outcome { session, kind });
    }
    if report.acted() > 0 {
        database.process_cleanup_jobs()?;
    }
    Ok(report)
}

/// Reclaims space that no session references any more: due cleanup jobs,
/// orphaned artifact directories past their grace period, the snapshot
/// directories from before change records, the change stores (see
/// [`StorePrune`]), the WAL, and freelist pages. Session rows are never
/// touched.
pub fn prune(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    store_budget: NonZeroU64,
    dry_run: bool,
) -> Result<PruneReport, SessionError> {
    let mut report = PruneReport {
        dry_run,
        cleanup_jobs_due: database.due_cleanup_jobs()?,
        freelist_pages_before: database.freelist_pages()?,
        ..PruneReport::default()
    };
    if !dry_run {
        PRUNE.report(PruneEvent::Phase {
            label: CLEANUP_JOBS_PHASE,
        });
        report.cleanup_jobs_completed = database.process_cleanup_jobs()?;
    }
    PRUNE.report(PruneEvent::Phase {
        label: ORPHAN_SCAN_PHASE,
    });
    let known = database.persisted_session_ids()?;
    let now = SystemTime::now();
    let archives = state_dir.path().join(SESSIONS_DIR).join(ARCHIVE_DIR);
    let (directories, bytes) =
        remove_orphan_directories(state_dir, &archives, &known, now, dry_run)?;
    report.orphan_directories += directories;
    report.orphan_bytes += bytes;
    let (directories, bytes) = remove_legacy_snapshot_dirs(state_dir, dry_run)?;
    report.orphan_directories += directories;
    report.orphan_bytes += bytes;
    if let Some(stores) = database.change_stores.clone() {
        PRUNE.report(PruneEvent::Phase {
            label: CHANGE_STORES_PHASE,
        });
        StorePrune {
            stores: stores.as_ref(),
            state_dir,
            known: known.iter().copied().collect(),
            covered: database.record_coverage_stores()?,
            now,
            budget: store_budget,
            dry_run,
        }
        .run(&mut report)?;
    }
    PRUNE.report(PruneEvent::Phase {
        label: TOOL_OUTPUT_PHASE,
    });
    let store = ToolOutputStore::new(state_dir.clone());
    let orphan_outputs = if dry_run {
        store.count_orphans(&known)
    } else {
        store
            .cleanup_orphans(&known)
            .map(|count| u64::try_from(count).unwrap_or(u64::MAX))
    };
    report.orphan_tool_output_entries =
        orphan_outputs.map_err(|error| SessionError::ToolOutputCleanup(Box::new(error)))?;
    if !dry_run {
        PRUNE.report(PruneEvent::Phase {
            label: CHECKPOINT_PHASE,
        });
        report.checkpoint = Some(database.checkpoint(true)?);
        let pages = u32::try_from(report.freelist_pages_before).unwrap_or(u32::MAX);
        database.incremental_vacuum(pages)?;
    }
    report.freelist_pages_after = database.freelist_pages()?;
    Ok(report)
}

/// Runs trim, forget, and prune once when the interval has elapsed since the
/// last recorded sweep. Returns `None` when nothing was due or another
/// process holds the sweep.
pub fn sweep_if_due(
    state_dir: &StateDir,
    policy: &SweepPolicy,
    now: &Zoned,
) -> Result<Option<SweepReport>, SessionError> {
    if policy.interval.is_zero() {
        return Ok(None);
    }
    let Some(_sweep_lock) =
        try_exclusive_state_lock(&state_dir.path().join(SWEEP_LOCK_FILE), OWNER_FILE_MODE)?
    else {
        return Ok(None);
    };
    let mut database = SessionDatabase::open(state_dir)?;
    let now_epoch = u64::try_from(now.timestamp().as_second()).unwrap_or(0);
    let last: Option<u64> = database.global_state_get(LAST_SWEEP_KEY)?;
    if last.is_some_and(|last| now_epoch.saturating_sub(last) < policy.interval.as_secs()) {
        return Ok(None);
    }
    let mut report = SweepReport::default();
    if !policy.trim.is_empty() {
        let plan = plan(
            &database,
            Action::Trim,
            policy.trim,
            policy.group_by,
            None,
            now,
        )?;
        report.trim = execute(&mut database, state_dir, &plan)?;
    }
    if !policy.forget.is_empty() {
        let plan = plan(
            &database,
            Action::Forget,
            policy.forget,
            policy.group_by,
            None,
            now,
        )?;
        report.forget = execute(&mut database, state_dir, &plan)?;
    }
    report.prune = prune(&mut database, state_dir, policy.store_budget, false)?;
    database.global_state_set(LAST_SWEEP_KEY, &now_epoch)?;
    info!(
        trimmed = report.trim.acted(),
        forgotten = report.forget.acted(),
        failed = report.trim.failed() + report.forget.failed(),
        released_bytes = report.trim.released_bytes() + report.forget.released_bytes(),
        "retention sweep finished"
    );
    Ok(Some(report))
}

/// Removes `<root>/<session id>` directories whose session is unknown and
/// which are older than the grace period. Returns directories and bytes,
/// counted rather than removed in a dry run.
fn remove_orphan_directories(
    state_dir: &StateDir,
    root: &Path,
    known: &[CaudraId],
    now: SystemTime,
    dry_run: bool,
) -> Result<(u64, u64), SessionError> {
    let mut directories = 0;
    let mut bytes = 0;
    for (path, metadata) in child_directories(root)? {
        if !is_orphan(&path, &metadata, known, now)? {
            continue;
        }
        directories += 1;
        bytes += directory_bytes(&path);
        if !dry_run {
            let _artifact_lock = lock_session_artifacts(state_dir)?;
            fs::remove_dir_all(&path).map_err(StorageError::from)?;
        }
    }
    Ok((directories, bytes))
}

/// Removes the snapshot directories from before change records whole. Nothing
/// reads them, so they need no grace period. Returns directories and bytes,
/// counted rather than removed in a dry run.
fn remove_legacy_snapshot_dirs(
    state_dir: &StateDir,
    dry_run: bool,
) -> Result<(u64, u64), SessionError> {
    let mut directories = 0;
    let mut bytes = 0;
    for (path, _) in child_directories(state_dir.path())? {
        if !path
            .file_name()
            .is_some_and(|name| LEGACY_SNAPSHOT_DIRS.iter().any(|legacy| name == *legacy))
        {
            continue;
        }
        directories += 1;
        bytes += directory_bytes(&path);
        if !dry_run {
            fs::remove_dir_all(&path).map_err(StorageError::from)?;
        }
    }
    Ok((directories, bytes))
}

/// One pass over the local change stores. In each store it releases every
/// holder whose session the database does not know, once the holder's id is
/// past the grace period, then cleans the store down to `budget`, then removes
/// the store when it keeps nothing, no session's file revert reads it, and it
/// was last written before the grace period. A holder that is not a session id
/// is never released. A store that fails is counted and skipped.
struct StorePrune<'a> {
    stores: &'a dyn ChangeStores,
    state_dir: &'a StateDir,
    known: HashSet<CaudraId>,
    /// An empty store still tells these sessions which of their records
    /// retention evicted.
    covered: HashSet<String>,
    now: SystemTime,
    budget: NonZeroU64,
    dry_run: bool,
}

impl StorePrune<'_> {
    fn run(&self, report: &mut PruneReport) -> Result<(), SessionError> {
        let keys = self
            .stores
            .keys(self.state_dir)
            .map_err(StorageError::from)?;
        for key in keys {
            if let Err(error) = self.prune(&key, report) {
                warn!(store = key, %error, "change store not pruned");
                report.change_store_failures += 1;
            }
        }
        Ok(())
    }

    fn prune(&self, key: &str, report: &mut PruneReport) -> Result<(), SessionError> {
        let store = self.state_dir.path().join(WORKSPACE_CHANGES_DIR).join(key);
        // Read first: releasing and cleaning both rewrite the state file.
        let idle = match fs::symlink_metadata(store.join(CHANGE_STORE_STATE_FILE)) {
            Ok(metadata) => is_past_grace(&metadata, self.now)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(StorageError::from(error).into()),
        };
        let holders = self
            .stores
            .holders(self.state_dir, key)
            .map_err(StorageError::from)?;
        for summary in holders {
            if !self.is_orphan(&summary.holder) {
                continue;
            }
            report.orphan_record_holders += 1;
            if !self.dry_run {
                self.stores
                    .release(self.state_dir, key, &summary.holder)
                    .map_err(StorageError::from)?;
            }
        }
        report.record_bytes_reclaimed += self
            .stores
            .clean_up(self.state_dir, key, self.budget, self.dry_run)
            .map_err(StorageError::from)?;
        // Holders hold records, open records, or pending reverts, so a store
        // that keeps none of them has no holder either.
        if !idle
            || self.covered.contains(key)
            || !self
                .stores
                .usage(self.state_dir, key)
                .map_err(StorageError::from)?
                .keeps_nothing()
        {
            return Ok(());
        }
        report.orphan_directories += 1;
        report.orphan_bytes += directory_bytes(&store);
        if !self.dry_run {
            fs::remove_dir_all(&store).map_err(StorageError::from)?;
        }
        Ok(())
    }

    fn is_orphan(&self, holder: &RecordHolder) -> bool {
        let Ok(id) = holder.as_str().parse::<CaudraId>() else {
            return false;
        };
        !self.known.contains(&id)
            && history_uuid_timestamp(id).is_some_and(|millis| {
                self.now
                    .duration_since(UNIX_EPOCH + Duration::from_millis(millis))
                    .is_ok_and(|age| age >= ORPHAN_GRACE)
            })
    }
}

/// The real directories directly in `dir`, none when it is absent. A symlink
/// is never one: removal must not follow it out of the state directory.
fn child_directories(dir: &Path) -> Result<Vec<(PathBuf, fs::Metadata)>, SessionError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(StorageError::from(error).into()),
    };
    let mut directories = Vec::new();
    for entry in entries {
        let path = entry.map_err(StorageError::from)?.path();
        let metadata = fs::symlink_metadata(&path).map_err(StorageError::from)?;
        if metadata.is_dir() {
            directories.push((path, metadata));
        }
    }
    Ok(directories)
}

/// A directory named for a session the database does not know, and old enough
/// that it cannot be one whose first save is still to commit.
fn is_orphan(
    path: &Path,
    metadata: &fs::Metadata,
    known: &[CaudraId],
    now: SystemTime,
) -> Result<bool, SessionError> {
    let Some(id) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<CaudraId>().ok())
    else {
        return Ok(false);
    };
    Ok(!known.contains(&id) && is_past_grace(metadata, now)?)
}

fn is_past_grace(metadata: &fs::Metadata, now: SystemTime) -> Result<bool, SessionError> {
    let modified = metadata.modified().map_err(StorageError::from)?;
    Ok(now
        .duration_since(modified)
        .is_ok_and(|age| age >= ORPHAN_GRACE))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jiff::civil::date;
    use jiff::tz::TimeZone;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::retention::Duration as RetentionDuration;
    use crate::sessions::change_stores::StoreUsage;
    use crate::sessions::change_stores::fake::{FakeStore, FakeStores};
    use crate::sessions::{RecordCoverage, Session, TitleSource};

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const OPEN_SESSION_SKIPPED: &str = "an open session must be skipped, not trimmed";
    const PLAN_IS_READ_ONLY: &str = "planning must not modify the repository";
    const PREVIEW_IS_READ_ONLY: &str = "a dry run must not change anything";
    const ONLY_STALE_ORPHANS: &str =
        "only a session id past the grace period with no session may be released";
    const CLEANED_TO_BUDGET: &str = "every store must be cleaned to the budget, once";
    const EMPTY_STALE_STORES_GO: &str = "a store goes once it keeps nothing, no session's file \
                                         revert reads it, and it was last written before the \
                                         grace period";
    const LEGACY_REMOVED: &str = "the snapshot directories go whole, whatever their age";
    const STORE: &str = "workspace-key";
    const STORE_KEYS: [&str; 2] = [STORE, "other-workspace-key"];
    const STORE_BUDGET: NonZeroU64 = NonZeroU64::new(64 * 1024 * 1024).unwrap();
    /// How far past the grace period an old artifact is.
    const MARGIN: Duration = Duration::from_secs(60);
    /// The bytes of a UUIDv7 that carry its millisecond timestamp.
    const UUID_TIMESTAMP_BYTES: usize = 6;

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct TestMessage(String);

    impl TitleSource for TestMessage {
        fn first_user_text(&self) -> Option<&str> {
            Some(&self.0)
        }
    }

    type TestSession = Session<TestMessage, Value, Value>;

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, state_dir)
    }

    fn now() -> Zoned {
        date(2025, 5, 3)
            .at(12, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    fn epoch(now: &Zoned, days_ago: i64) -> u64 {
        u64::try_from(now.timestamp().as_second() - days_ago * 24 * 60 * 60).unwrap()
    }

    fn saved_session(database: &mut SessionDatabase, cwd: &str, updated_at: u64) -> TestSession {
        let mut session = TestSession::new(MODEL, cwd);
        session.push_message(TestMessage("prompt".into()));
        session.insert_tool_output("big".into(), json!({"text": "x".repeat(8192)}));
        session.updated_at = updated_at;
        session.created_at = updated_at;
        database.save(&session, None).unwrap();
        session
    }

    fn keep_last(count: u32) -> KeepPolicy {
        KeepPolicy {
            keep_last: Some(count),
            ..KeepPolicy::default()
        }
    }

    /// A session id generated before the grace period.
    fn stale_id() -> CaudraId {
        let generated = SystemTime::now() - ORPHAN_GRACE - MARGIN;
        let millis = generated.duration_since(UNIX_EPOCH).unwrap().as_millis();
        let millis = u64::try_from(millis).unwrap().to_be_bytes();
        let mut bytes = *CaudraId::generate().as_bytes();
        bytes[..UUID_TIMESTAMP_BYTES]
            .copy_from_slice(&millis[millis.len() - UUID_TIMESTAMP_BYTES..]);
        CaudraId::from_bytes(bytes)
    }

    fn with_stores(database: &mut SessionDatabase, stores: &Arc<FakeStores>) {
        database.change_stores = Some(stores.clone());
    }

    #[test]
    fn plan_reports_keep_act_and_skip_without_writing() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let now = now();
        let newest = saved_session(&mut database, CWD, epoch(&now, 1));
        let old = saved_session(&mut database, CWD, epoch(&now, 200));
        let mut reverting = TestSession::new(MODEL, CWD);
        reverting.meta.pending_revert = Some(crate::sessions::PendingConversationRevert {
            original_head: None,
            target_head: None,
            file_status: None,
        });
        reverting.updated_at = epoch(&now, 300);
        database.save(&reverting, None).unwrap();
        let before = database.stats().unwrap();

        let plan = plan(
            &database,
            Action::Trim,
            keep_last(1),
            GroupBy::Directory,
            None,
            &now,
        )
        .unwrap();

        assert_eq!(plan.groups.len(), 1);
        let group = &plan.groups[0];
        assert_eq!(group.key, CWD);
        assert_eq!(group.keep.len(), 1);
        assert_eq!(group.keep[0].session.id, newest.id);
        assert_eq!(group.act.len(), 1);
        assert_eq!(group.act[0].session.id, old.id);
        assert_eq!(group.skip.len(), 1);
        assert_eq!(group.skip[0].session.id, reverting.id);
        assert_eq!(group.skip[0].reason, Skip::PendingRevert);
        assert_eq!(
            database.stats().unwrap().logical_bytes,
            before.logical_bytes,
            "{PLAN_IS_READ_ONLY}"
        );
    }

    #[test]
    fn execute_trims_candidates_and_skips_open_sessions() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let now = now();
        let _newest = saved_session(&mut database, CWD, epoch(&now, 1));
        let old = saved_session(&mut database, CWD, epoch(&now, 200));
        let open = saved_session(&mut database, CWD, epoch(&now, 400));
        let lease = SessionLease::acquire(&state_dir, open.id).unwrap();
        let plan = plan(
            &database,
            Action::Trim,
            keep_last(1),
            GroupBy::None,
            None,
            &now,
        )
        .unwrap();
        assert_eq!(plan.candidate_count(), 2);

        let report = execute(&mut database, &state_dir, &plan).unwrap();

        assert_eq!(report.acted(), 1);
        assert_eq!(report.failed(), 0);
        let skipped = report
            .outcomes
            .iter()
            .find(|outcome| outcome.session.id == open.id)
            .unwrap();
        assert!(
            matches!(skipped.kind, OutcomeKind::Skipped { reason } if reason == SKIP_OPEN),
            "{OPEN_SESSION_SKIPPED}"
        );
        drop(lease);
        let facts = database.session_facts(None).unwrap();
        let trimmed = facts.iter().find(|facts| facts.id == old.id).unwrap();
        assert!(trimmed.is_trimmed());
        let untouched = facts.iter().find(|facts| facts.id == open.id).unwrap();
        assert!(!untouched.is_trimmed());

        let again = super::plan(
            &database,
            Action::Trim,
            keep_last(1),
            GroupBy::None,
            None,
            &now,
        )
        .unwrap();
        assert_eq!(again.candidate_count(), 1);
        assert_eq!(again.groups[0].skip[0].reason, Skip::AlreadyTrimmed);
    }

    #[test]
    fn execute_forgets_candidates_and_drains_cleanup() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let now = now();
        let newest = saved_session(&mut database, CWD, epoch(&now, 1));
        let old = saved_session(&mut database, CWD, epoch(&now, 200));
        with_stores(&mut database, &Arc::default());
        let plan = plan(
            &database,
            Action::Forget,
            keep_last(1),
            GroupBy::None,
            None,
            &now,
        )
        .unwrap();

        let report = execute(&mut database, &state_dir, &plan).unwrap();

        assert_eq!(report.acted(), 1);
        let remaining = database.persisted_session_ids().unwrap();
        assert_eq!(remaining, vec![newest.id]);
        assert!(database.tombstone_version(old.id).unwrap().is_some());
        assert_eq!(database.stats().unwrap().pending_cleanup_jobs, 0);
    }

    #[test]
    fn forget_ids_refuses_pinned_and_unknown_sessions() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let now = now();
        let pinned = saved_session(&mut database, CWD, epoch(&now, 1));
        let plain = saved_session(&mut database, CWD, epoch(&now, 2));
        database.set_pinned(pinned.id, true).unwrap();

        let report = apply_ids(
            &mut database,
            &state_dir,
            Action::Forget,
            &[pinned.id, plain.id],
        )
        .unwrap();

        assert_eq!(report.acted(), 1);
        assert!(matches!(
            report.outcomes[0].kind,
            OutcomeKind::Skipped { reason } if reason == SKIP_PINNED
        ));
        assert_eq!(database.persisted_session_ids().unwrap(), vec![pinned.id]);
        assert!(matches!(
            apply_ids(
                &mut database,
                &state_dir,
                Action::Forget,
                &[CaudraId::generate()]
            ),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
    }

    /// Naming a session must reclaim its artifacts without costing the
    /// conversation: that is the whole reason to trim one session by hand
    /// rather than forget it.
    #[test]
    fn trimming_by_id_releases_the_change_records_and_keeps_the_session() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = saved_session(&mut database, CWD, epoch(&now(), 1));
        let held = FakeStore {
            holders: vec![session.id.to_string()],
            ..FakeStore::default()
        };
        let stores = Arc::new(FakeStores::with([(STORE, held)]));
        with_stores(&mut database, &stores);

        let report = apply_ids(&mut database, &state_dir, Action::Trim, &[session.id]).unwrap();

        assert_eq!(report.acted(), 1);
        assert!(stores.store(STORE).holders.is_empty());
        assert_eq!(
            database.persisted_session_ids().unwrap(),
            vec![session.id],
            "trimming must keep the session it reclaimed"
        );
    }

    #[test]
    fn prune_removes_stale_orphans_and_keeps_fresh_and_known_ones() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let now = now();
        let known = saved_session(&mut database, CWD, epoch(&now, 1));
        let archives = state_dir.path().join(SESSIONS_DIR).join(ARCHIVE_DIR);
        let known_dir = archives.join(known.id.to_string());
        let stale_dir = archives.join(CaudraId::generate().to_string());
        let fresh_dir = archives.join(CaudraId::generate().to_string());
        for dir in [&known_dir, &stale_dir, &fresh_dir] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("object"), b"bytes").unwrap();
        }
        fs::File::open(&stale_dir)
            .unwrap()
            .set_modified(SystemTime::now() - ORPHAN_GRACE - MARGIN)
            .unwrap();

        let preview = prune(&mut database, &state_dir, STORE_BUDGET, true).unwrap();
        assert!(preview.dry_run);
        assert_eq!(preview.orphan_directories, 1);
        assert_eq!(preview.orphan_bytes, 5);
        assert!(stale_dir.exists());

        let report = prune(&mut database, &state_dir, STORE_BUDGET, false).unwrap();

        assert_eq!(report.orphan_directories, 1);
        assert!(!stale_dir.exists());
        assert!(fresh_dir.exists());
        assert!(known_dir.exists());
        assert!(report.checkpoint.is_some());
    }

    #[test]
    fn prune_removes_the_snapshot_directories_from_before_change_records() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let legacy = LEGACY_SNAPSHOT_DIRS.map(|name| state_dir.path().join(name).join(STORE));
        for dir in &legacy {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("object"), b"bytes").unwrap();
        }

        let preview = prune(&mut database, &state_dir, STORE_BUDGET, true).unwrap();
        assert_eq!(preview.orphan_directories, legacy.len() as u64);
        assert!(
            legacy.iter().all(|dir| dir.exists()),
            "{PREVIEW_IS_READ_ONLY}"
        );

        prune(&mut database, &state_dir, STORE_BUDGET, false).unwrap();

        for name in LEGACY_SNAPSHOT_DIRS {
            assert!(!state_dir.path().join(name).exists(), "{LEGACY_REMOVED}");
        }
    }

    #[test]
    fn prune_releases_only_stale_holders_without_a_session() {
        const NOT_A_SESSION: &str = "not-a-session";
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut live = TestSession::new(MODEL, CWD);
        live.id = stale_id();
        database.save(&live, None).unwrap();
        let kept = [
            live.id.to_string(),
            CaudraId::generate().to_string(),
            NOT_A_SESSION.to_owned(),
        ];
        let held = FakeStore {
            holders: [stale_id().to_string()]
                .into_iter()
                .chain(kept.clone())
                .collect(),
            ..FakeStore::default()
        };
        let stores = Arc::new(FakeStores::with([(STORE, held.clone())]));
        with_stores(&mut database, &stores);

        let preview = prune(&mut database, &state_dir, STORE_BUDGET, true).unwrap();
        assert_eq!(preview.orphan_record_holders, 1);
        assert_eq!(
            stores.store(STORE).holders,
            held.holders,
            "{PREVIEW_IS_READ_ONLY}"
        );

        let report = prune(&mut database, &state_dir, STORE_BUDGET, false).unwrap();

        assert_eq!(report.orphan_record_holders, 1);
        assert_eq!(stores.store(STORE).holders, kept, "{ONLY_STALE_ORPHANS}");
    }

    #[test]
    fn prune_cleans_every_store_down_to_the_budget() {
        const RECLAIMABLE: u64 = 4096;
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let store = FakeStore {
            reclaimable: RECLAIMABLE,
            ..FakeStore::default()
        };
        let stores = Arc::new(FakeStores::with(STORE_KEYS.map(|key| (key, store.clone()))));
        with_stores(&mut database, &stores);
        let reclaimable = RECLAIMABLE * STORE_KEYS.len() as u64;

        let preview = prune(&mut database, &state_dir, STORE_BUDGET, true).unwrap();
        assert_eq!(preview.record_bytes_reclaimed, reclaimable);
        let report = prune(&mut database, &state_dir, STORE_BUDGET, false).unwrap();

        assert_eq!(report.record_bytes_reclaimed, reclaimable);
        for key in STORE_KEYS {
            assert_eq!(
                stores.store(key).cleaned_to,
                [STORE_BUDGET.get()],
                "{CLEANED_TO_BUDGET}"
            );
        }
    }

    #[test_case(true, 0, false, true ; "an empty store last written before the grace period goes")]
    #[test_case(false, 0, false, false ; "a store written within the grace period stays")]
    #[test_case(true, 1, false, false ; "a store that keeps records stays")]
    #[test_case(true, 0, true, false ; "a store a session's file revert reads stays")]
    fn prune_removes_a_store_once_it_is_empty_and_stale(
        stale: bool,
        records: u32,
        covered: bool,
        removed: bool,
    ) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        if covered {
            let mut session = TestSession::new(MODEL, CWD);
            session.push_message(TestMessage("prompt".into()));
            session.meta.record_coverage = Some(RecordCoverage {
                store: STORE.to_owned(),
                since: None,
            });
            database.save(&session, None).unwrap();
        }
        let path = state_dir.path().join(WORKSPACE_CHANGES_DIR).join(STORE);
        fs::create_dir_all(&path).unwrap();
        let state = path.join(CHANGE_STORE_STATE_FILE);
        fs::write(&state, b"state").unwrap();
        if stale {
            fs::File::open(&state)
                .unwrap()
                .set_modified(SystemTime::now() - ORPHAN_GRACE - MARGIN)
                .unwrap();
        }
        let store = FakeStore {
            usage: StoreUsage {
                records,
                ..StoreUsage::default()
            },
            ..FakeStore::default()
        };
        with_stores(&mut database, &Arc::new(FakeStores::with([(STORE, store)])));

        let report = prune(&mut database, &state_dir, STORE_BUDGET, false).unwrap();

        assert_eq!(path.exists(), !removed, "{EMPTY_STALE_STORES_GO}");
        assert_eq!(report.orphan_directories, u64::from(removed));
    }

    #[test]
    fn sweep_runs_once_per_interval_and_records_the_run() {
        let (_temp, state_dir) = state_dir();
        let now = now();
        {
            let mut database = SessionDatabase::open(&state_dir).unwrap();
            saved_session(&mut database, CWD, epoch(&now, 1));
            saved_session(&mut database, CWD, epoch(&now, 200));
        }
        let policy = SweepPolicy {
            group_by: GroupBy::Directory,
            interval: Duration::from_secs(60 * 60),
            trim: KeepPolicy {
                keep_within: Some(RetentionDuration {
                    days: 90,
                    ..RetentionDuration::default()
                }),
                ..KeepPolicy::default()
            },
            forget: KeepPolicy::default(),
            store_budget: STORE_BUDGET,
        };

        let first = sweep_if_due(&state_dir, &policy, &now).unwrap().unwrap();
        assert_eq!(first.trim.acted(), 1);
        assert_eq!(first.forget.acted(), 0);

        assert!(sweep_if_due(&state_dir, &policy, &now).unwrap().is_none());
        let later = now.checked_add(jiff::Span::new().hours(2)).unwrap();
        let second = sweep_if_due(&state_dir, &policy, &later).unwrap().unwrap();
        assert_eq!(second.trim.acted(), 0);
    }

    #[test]
    fn sweep_is_disabled_by_a_zero_interval() {
        let (_temp, state_dir) = state_dir();
        let policy = SweepPolicy {
            group_by: GroupBy::Directory,
            interval: Duration::ZERO,
            trim: keep_last(1),
            forget: KeepPolicy::default(),
            store_budget: STORE_BUDGET,
        };
        assert!(sweep_if_due(&state_dir, &policy, &now()).unwrap().is_none());
    }
}
