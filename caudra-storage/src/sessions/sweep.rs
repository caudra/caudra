//! Plans and executes retention over the session repository. The CLI and
//! the background sweep share this code so a dry run shows exactly what the
//! sweep would do.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use jiff::Zoned;
use serde::Serialize;
use tracing::{info, warn};

use super::database::SESSION_SNAPSHOT_DIR;
use super::lease::SessionLease;
use super::{
    ARCHIVE_DIR, CheckpointResult, SESSIONS_DIR, SessionDatabase, SessionError, TrimReport,
};
use crate::id::CaudraId;
use crate::retention::{self, Decision, GroupBy, KeepPolicy, SessionFacts};
use crate::tool_outputs::ToolOutputStore;
use crate::{StateDir, StorageError, lock_session_artifacts, try_exclusive_state_lock};

const SWEEP_LOCK_FILE: &str = "sessions.sqlite3.sweep.lock";
const LAST_SWEEP_KEY: &str = "retention.last_sweep_at";
const OWNER_FILE_MODE: u32 = 0o600;
/// Orphaned artifact directories younger than this may belong to a session
/// whose first save has not committed yet.
const ORPHAN_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
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

/// Deletes the sessions in `ids` regardless of policy. Pinned and open
/// sessions are refused.
pub fn forget_ids(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
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
            act_on(database, state_dir, Action::Forget, &candidate)
        };
        report.outcomes.push(Outcome { session, kind });
    }
    if report.acted() > 0 {
        database.process_cleanup_jobs()?;
    }
    Ok(report)
}

/// Reclaims space that no session references any more: due cleanup jobs,
/// orphaned artifact directories past their grace period, the WAL, and
/// freelist pages. Session rows are never touched.
pub fn prune(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    dry_run: bool,
) -> Result<PruneReport, SessionError> {
    let mut report = PruneReport {
        dry_run,
        cleanup_jobs_due: database.due_cleanup_jobs()?,
        freelist_pages_before: database.freelist_pages()?,
        ..PruneReport::default()
    };
    if !dry_run {
        report.cleanup_jobs_completed = database.process_cleanup_jobs()?;
    }
    let known = database.persisted_session_ids()?;
    let now = SystemTime::now();
    for components in [
        [SESSION_SNAPSHOT_DIR].as_slice(),
        [SESSIONS_DIR, ARCHIVE_DIR].as_slice(),
    ] {
        let mut root = state_dir.path().to_path_buf();
        root.extend(components);
        let (directories, bytes) =
            remove_orphan_directories(state_dir, &root, &known, now, dry_run)?;
        report.orphan_directories += directories;
        report.orphan_bytes += bytes;
    }
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
    report.prune = prune(&mut database, state_dir, false)?;
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
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(StorageError::from(error).into()),
    };
    let mut directories = 0;
    let mut bytes = 0;
    for entry in entries {
        let entry = entry.map_err(StorageError::from)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(StorageError::from)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<CaudraId>().ok())
        else {
            continue;
        };
        if known.contains(&id) {
            continue;
        }
        let modified = metadata.modified().map_err(StorageError::from)?;
        if !now
            .duration_since(modified)
            .is_ok_and(|age| age >= ORPHAN_GRACE)
        {
            continue;
        }
        directories += 1;
        bytes += super::database::directory_bytes(&path);
        if !dry_run {
            let _artifact_lock = lock_session_artifacts(state_dir)?;
            fs::remove_dir_all(&path).map_err(StorageError::from)?;
        }
    }
    Ok((directories, bytes))
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use jiff::tz::TimeZone;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::retention::Duration as RetentionDuration;
    use crate::sessions::{Session, TitleSource};

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const OPEN_SESSION_SKIPPED: &str = "an open session must be skipped, not trimmed";
    const PLAN_IS_READ_ONLY: &str = "planning must not modify the repository";

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
            original_workspace_head: None,
            workspace_head: None,
            file_status: None,
            restore_operation: None,
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

        let report = forget_ids(&mut database, &state_dir, &[pinned.id, plain.id]).unwrap();

        assert_eq!(report.acted(), 1);
        assert!(matches!(
            report.outcomes[0].kind,
            OutcomeKind::Skipped { reason } if reason == SKIP_PINNED
        ));
        assert_eq!(database.persisted_session_ids().unwrap(), vec![pinned.id]);
        assert!(matches!(
            forget_ids(&mut database, &state_dir, &[CaudraId::generate()]),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
    }

    #[test]
    fn prune_removes_stale_orphans_and_keeps_fresh_and_known_ones() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let now = now();
        let known = saved_session(&mut database, CWD, epoch(&now, 1));
        let snapshots = state_dir.path().join(SESSION_SNAPSHOT_DIR);
        let known_dir = snapshots.join(known.id.to_string());
        let stale_dir = snapshots.join(CaudraId::generate().to_string());
        let fresh_dir = snapshots.join(CaudraId::generate().to_string());
        for dir in [&known_dir, &stale_dir, &fresh_dir] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("object"), b"bytes").unwrap();
        }
        let old = SystemTime::now() - ORPHAN_GRACE - Duration::from_secs(60);
        fs::File::open(&stale_dir)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let preview = prune(&mut database, &state_dir, true).unwrap();
        assert!(preview.dry_run);
        assert_eq!(preview.orphan_directories, 1);
        assert_eq!(preview.orphan_bytes, 5);
        assert!(stale_dir.exists());

        let report = prune(&mut database, &state_dir, false).unwrap();

        assert_eq!(report.orphan_directories, 1);
        assert!(!stale_dir.exists());
        assert!(fresh_dir.exists());
        assert!(known_dir.exists());
        assert!(report.checkpoint.is_some());
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
        };
        assert!(sweep_if_due(&state_dir, &policy, &now()).unwrap().is_none());
    }
}
