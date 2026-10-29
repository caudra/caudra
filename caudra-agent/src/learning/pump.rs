//! When a learner runs. A runtime keeps one [`Learner`] per project. Every
//! settled turn nudges it; once the session has been idle a while, it checks
//! whether enough has happened since the project's last run, takes the
//! project's lease so no other process learns it at once, and runs.
//!
//! The cursor moves only for a run that finished and applied all it could,
//! so a failed or conflicting run leaves its history for the next one.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use caudra_config::LearningTarget;
use caudra_storage::StateDir;
use caudra_storage::history_search::{HistoryRef, HistoryScope};
use caudra_storage::id::CaudraId;
use caudra_storage::learning::{FinishedRun, LearningProject, LearningRun, LearningStore};
use flume::{Receiver, Sender};
use futures_lite::future;
use smol::Task;
use thiserror::Error;
use tracing::{debug, info, warn};

use super::apply::{self, ApplyError, Evidence, Report};
use super::digest::{self, Digest};
use super::runner::{self, Shown, Spend};
use super::targets::{self, InstructionFile};
use super::{LearnedNotice, LearnerActivity, LearnerStep};
use crate::agent::task_runner::WorkflowHostContext;
use crate::history::{HistoryProject, HistoryStore};
use crate::memory::store::{MemoryStore, now_ms};
use crate::tools::LockKey;
use crate::types::AgentEvent;

pub const MIN_USER_MESSAGES: usize = 10;
pub const MIN_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);
pub const BACKLOG_INTERVAL: Duration = Duration::from_secs(10 * 60);
pub const IDLE_GRACE: Duration = Duration::from_secs(60);
pub const BOOTSTRAP_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const MAX_RUNS_PER_DAY: usize = 8;
/// While a project has no memory notes, the learner runs after this many
/// typed messages and [`BACKLOG_INTERVAL`] apart, so the first notes come
/// early and later sessions have examples to follow.
pub const EMPTY_MEMORY_USER_MESSAGES: usize = 3;
/// How many runs a day may take the empty-memory pace.
pub const EMPTY_MEMORY_RUNS_PER_DAY: usize = 2;
/// History this recent is left for the next run, so a turn still being
/// written is never read half done.
pub const SETTLE: Duration = Duration::from_secs(60);
/// Longer than any run takes, so a lease outlives its run only when the
/// process holding it died.
pub const LEASE: Duration = Duration::from_secs(30 * 60);
pub const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
pub const LEARNED: &str = "learned";
/// Finished, with more history left than one digest holds.
pub const PARTIAL: &str = "partial";
/// Finished, but a file changed under it, so its history is read again.
pub const CONFLICT: &str = "conflict";
pub const FAILED: &str = "failed";
pub const PAUSED: &str = "paused";
pub const DAILY_CAP: &str = "daily cap reached";
pub const TOO_SOON: &str = "too soon after the last run";
pub const TIMED_OUT: &str = "the learner timed out";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// Items one run reads, oldest first; the rest wait for the next.
const TIMELINE_LIMIT: usize = 20_000;

/// Whether a project's learner takes the empty-memory pace.
fn early(empty_memory: bool, runs_today: usize) -> bool {
    empty_memory && runs_today < EMPTY_MEMORY_RUNS_PER_DAY
}

/// How many typed messages a run waits for.
pub fn min_user_messages(empty_memory: bool, runs_today: usize) -> usize {
    match early(empty_memory, runs_today) {
        true => EMPTY_MEMORY_USER_MESSAGES,
        false => MIN_USER_MESSAGES,
    }
}

/// Why a project's learner waits rather than runs, `None` when it may run.
/// A forced run skips the spacing between runs but never a pause or the
/// daily cap.
pub fn waiting(
    project: Option<&LearningProject>,
    last: Option<&LearningRun>,
    runs_today: usize,
    now_ms: i64,
    forced: bool,
    empty_memory: bool,
) -> Option<&'static str> {
    if project.is_some_and(|project| project.paused) {
        return Some(PAUSED);
    }
    if runs_today >= MAX_RUNS_PER_DAY {
        return Some(DAILY_CAP);
    }
    let spacing = match last.map(|run| run.outcome.as_str()) {
        Some(PARTIAL) => BACKLOG_INTERVAL,
        _ if early(empty_memory, runs_today) => BACKLOG_INTERVAL,
        _ => MIN_INTERVAL,
    };
    let recent = last.is_some_and(|run| now_ms - run.started_ms < millis(spacing));
    (recent && !forced).then_some(TOO_SOON)
}

/// What a runtime gives its learner.
pub struct LearnerDeps {
    pub state: StateDir,
    pub cwd: PathBuf,
    /// Where the user's own `AGENTS.md` lives.
    pub config_dir: Option<PathBuf>,
    /// The session's task host, which [`runner::learner_host`] narrows.
    pub host: WorkflowHostContext,
    pub memory: Option<Arc<MemoryStore>>,
    pub targets: Vec<LearningTarget>,
    pub spend: Spend,
}

enum Wake {
    /// A turn ended: run once the session stays idle.
    Settled,
    /// A turn started: wait for it.
    Busy,
    /// Run now, whatever the spacing between runs.
    Now,
}

/// A project's learner. Dropping it stops a run in flight, which then
/// advances nothing.
pub struct Learner {
    wake: Sender<Wake>,
    project: String,
    _task: Task<()>,
}

impl Learner {
    pub fn start(deps: LearnerDeps) -> Self {
        let (wake, woken) = flume::unbounded();
        let shared = Arc::new(Shared::new(deps));
        Self {
            wake,
            project: shared.project.label.clone(),
            _task: smol::spawn(shared.run(woken)),
        }
    }

    /// A turn settled.
    pub fn nudge(&self) {
        let _ = self.wake.try_send(Wake::Settled);
    }

    /// A turn started.
    pub fn hold(&self) {
        let _ = self.wake.try_send(Wake::Busy);
    }

    /// What a frontend keeps to ask for a run.
    pub fn handle(&self) -> LearnerHandle {
        LearnerHandle {
            wake: self.wake.clone(),
            project: self.project.clone(),
        }
    }
}

/// Asks a [`Learner`] to run, while it lives.
#[derive(Clone)]
pub struct LearnerHandle {
    wake: Sender<Wake>,
    project: String,
}

impl LearnerHandle {
    /// Learn now, whatever the spacing between runs. False once the learner
    /// is gone.
    pub fn run_now(&self) -> bool {
        self.wake.try_send(Wake::Now).is_ok()
    }

    /// The project key the learner's runs are stored under.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// A handle with no learner behind it, and the probe that sees what it asked.
    #[cfg(any(test, feature = "test-support"))]
    pub fn detached(project: &str) -> (Self, LearnerProbe) {
        let (wake, woken) = flume::unbounded();
        let handle = Self {
            wake,
            project: project.to_owned(),
        };
        (handle, LearnerProbe(woken))
    }
}

#[cfg(any(test, feature = "test-support"))]
pub struct LearnerProbe(Receiver<Wake>);

#[cfg(any(test, feature = "test-support"))]
impl LearnerProbe {
    pub fn asked_to_run_now(&self) -> bool {
        self.0.try_iter().any(|wake| matches!(wake, Wake::Now))
    }
}

#[derive(Debug, Error)]
enum LearnError {
    #[error(transparent)]
    Session(#[from] caudra_storage::sessions::SessionError),
    #[error(transparent)]
    Apply(#[from] ApplyError),
    #[error(transparent)]
    Memory(#[from] crate::memory::store::MemoryError),
}

struct Shared {
    deps: LearnerDeps,
    host: Arc<WorkflowHostContext>,
    project: HistoryProject,
    root: PathBuf,
    owner: String,
}

impl Shared {
    fn new(deps: LearnerDeps) -> Self {
        let host = Arc::new(runner::learner_host(deps.host.clone()));
        Self {
            project: HistoryProject::local(&deps.cwd),
            root: targets::project_root(&deps.cwd),
            owner: CaudraId::generate().to_string(),
            host,
            deps,
        }
    }

    async fn run(self: Arc<Self>, woken: Receiver<Wake>) {
        let mut armed = false;
        loop {
            let wake = match armed {
                true => {
                    future::or(async { Some(woken.recv_async().await) }, async {
                        smol::Timer::after(IDLE_GRACE).await;
                        None
                    })
                    .await
                }
                false => Some(woken.recv_async().await),
            };
            let forced = match wake {
                Some(Err(_)) => return,
                Some(Ok(Wake::Settled)) => {
                    armed = true;
                    continue;
                }
                Some(Ok(Wake::Busy)) => {
                    armed = false;
                    continue;
                }
                Some(Ok(Wake::Now)) => true,
                None => false,
            };
            armed = false;
            if let Err(error) = self.attempt(forced).await {
                warn!(project = %self.project.label, %error, "continual learning run failed");
            }
        }
    }

    async fn attempt(&self, forced: bool) -> Result<(), LearnError> {
        let project = self.project.label.clone();
        let now = now_ms();
        let store = self.store().await?;
        let memory = self.memory();
        let (state, last, runs_today, empty_memory) = {
            let project = project.clone();
            smol::unblock(move || {
                let state = store.project(&project)?;
                let last = store.runs(&project, 1)?.into_iter().next();
                let today = store.runs_since(&project, now - millis(DAY))?;
                let empty = match memory {
                    Some(memory) => memory.live_count()? == 0,
                    None => false,
                };
                Ok::<_, LearnError>((state, last, today, empty))
            })
            .await?
        };
        if let Some(reason) = waiting(
            state.as_ref(),
            last.as_ref(),
            runs_today,
            now,
            forced,
            empty_memory,
        ) {
            debug!(%project, reason, "continual learning waits");
            return Ok(());
        }
        let scope = HistoryScope::Project(self.project.scope.clone());
        let history = HistoryStore::new(self.deps.state.clone());
        if let Err(error) = history.catch_up(scope.clone()).await {
            warn!(%project, %error, "history index could not catch up before learning");
        }
        let first_from = now - millis(BOOTSTRAP_WINDOW);
        let from = state
            .as_ref()
            .map(|state| state.through_ms)
            .filter(|through| *through > 0)
            .unwrap_or(first_from);
        let until = now - millis(SETTLE);
        let items = {
            let scope = scope.clone();
            history
                .read(move |database| {
                    database.history_timeline(
                        &scope,
                        &digest::KINDS,
                        unsigned(from),
                        unsigned(until),
                        TIMELINE_LIMIT,
                    )
                })
                .await?
        };
        let typed = digest::authored_messages(&items);
        if typed < min_user_messages(empty_memory, runs_today) && !forced {
            debug!(%project, typed, "continual learning waits for more messages");
            return Ok(());
        }
        let store = self.store().await?;
        let begun = {
            let (project, owner) = (project.clone(), self.owner.clone());
            smol::unblock(move || store.begin(&project, &owner, now, millis(LEASE), first_from))
                .await?
        };
        let Some(begun) = begun else {
            debug!(%project, "another process is learning this project");
            return Ok(());
        };
        let window_end = match items.len() >= TIMELINE_LIMIT {
            true => items.last().map_or(unsigned(until), |item| item.at_ms),
            false => unsigned(until),
        };
        let mut digest = Digest::build(&items, window_end);
        digest.truncated |= window_end < unsigned(until);
        info!(%project, run = begun.id, typed, empty_memory, exchanges = digest.exchanges, truncated = digest.truncated, "continual learning run started");
        let finished = match digest.exchanges {
            0 => FinishedRun {
                outcome: LEARNED.to_owned(),
                model: String::new(),
                tokens: 0,
                summary: String::new(),
                dropped: "[]".to_owned(),
                through_ms: Some(signed(digest.through_ms)),
                edits: Vec::new(),
                transcript: None,
            },
            _ => self.learn(begun.id, &digest, &scope).await,
        };
        let notice = notice(begun.id, &finished);
        let store = self.store().await?;
        let (project, owner) = (project.clone(), self.owner.clone());
        let outcome = finished.outcome.clone();
        smol::unblock(move || store.finish(&project, &owner, begun.id, now_ms(), &finished))
            .await?;
        info!(project = %self.project.label, run = begun.id, outcome, "continual learning run finished");
        self.announce(begun.id, LearnerStep::Finished { outcome });
        if notice.changed() {
            self.deps
                .spend
                .events
                .try_send(AgentEvent::Learned(Box::new(notice)));
        }
        Ok(())
    }

    /// The project's memory, when the learner may write to it.
    fn memory(&self) -> Option<Arc<MemoryStore>> {
        self.deps
            .memory
            .clone()
            .filter(|_| self.deps.targets.contains(&LearningTarget::Memory))
    }

    /// Runs the learner on `digest` and applies what passes.
    async fn learn(&self, run_id: i64, digest: &Digest, scope: &HistoryScope) -> FinishedRun {
        let files = self.files().await;
        let memory = self.memory();
        let memory_view = match &memory {
            Some(memory) => {
                let memory = Arc::clone(memory);
                smol::unblock(move || memory.view().map(|view| view.render()))
                    .await
                    .ok()
            }
            None => None,
        };
        let prompt = runner::prompt(&Shown {
            digest,
            memory_view,
            files: &files,
            targets: &self.deps.targets,
        });
        self.announce(
            run_id,
            LearnerStep::Started {
                prompt: prompt.clone(),
            },
        );
        let ran = future::or(
            async {
                Some(runner::run(Arc::clone(&self.host), run_id, prompt, &self.deps.spend).await)
            },
            async {
                smol::Timer::after(RUN_TIMEOUT).await;
                None
            },
        )
        .await;
        let failed =
            |summary: String, model: String, tokens: u64, transcript: Option<String>| FinishedRun {
                outcome: FAILED.to_owned(),
                model,
                tokens,
                summary,
                dropped: "[]".to_owned(),
                through_ms: None,
                edits: Vec::new(),
                transcript,
            };
        let Some(runner::Ran {
            report,
            model,
            tokens,
            transcript,
        }) = ran
        else {
            return failed(TIMED_OUT.to_owned(), String::new(), 0, None);
        };
        let report = match report {
            Ok(report) => report,
            Err(error) => return failed(error, model, tokens, transcript),
        };
        match self.apply(run_id, &report, files, memory, scope).await {
            Ok((applied, dropped)) => FinishedRun {
                outcome: match (applied.conflicts.is_empty(), digest.truncated) {
                    (false, _) => CONFLICT,
                    (true, true) => PARTIAL,
                    (true, false) => LEARNED,
                }
                .to_owned(),
                model,
                tokens,
                summary: report.summary.trim().to_owned(),
                dropped,
                through_ms: applied
                    .conflicts
                    .is_empty()
                    .then_some(signed(digest.through_ms)),
                edits: applied.edits,
                transcript,
            },
            Err(error) => failed(error.to_string(), model, tokens, transcript),
        }
    }

    async fn apply(
        &self,
        run_id: i64,
        report: &Report,
        files: Vec<InstructionFile>,
        memory: Option<Arc<MemoryStore>>,
        scope: &HistoryScope,
    ) -> Result<(apply::Applied, String), LearnError> {
        let evidence = self.evidence(report, scope).await?;
        let plan = apply::validate(report, &files, memory.is_some(), &evidence);
        let writes: Vec<LockKey> = files
            .iter()
            .map(|file| LockKey::Local(file.path.clone()))
            .collect();
        let _guards = self.host.path_locks.acquire(&writes, &[]).await;
        let dropped = serde_json::to_string(&plan.dropped).unwrap_or_default();
        let applied =
            smol::unblock(move || apply::apply(&plan, &files, memory.as_deref(), run_id)).await?;
        Ok((applied, dropped))
    }

    /// The cited references that name a stored session of this project.
    async fn evidence(
        &self,
        report: &Report,
        scope: &HistoryScope,
    ) -> Result<Evidence, LearnError> {
        let cited: Vec<HistoryRef> = report
            .cited()
            .iter()
            .filter_map(|cited| cited.parse().ok())
            .collect();
        let scope = scope.clone();
        let history = HistoryStore::new(self.deps.state.clone());
        let resolved = history
            .read(move |database| {
                let mut resolved = Vec::new();
                for reference in cited {
                    if database
                        .history_session(&scope, reference.session)?
                        .is_some()
                    {
                        resolved.push(reference);
                    }
                }
                Ok(resolved)
            })
            .await?;
        Ok(Evidence::new(resolved))
    }

    /// The instruction files of the enabled targets, as they are now. The
    /// local file is ignored by version control before it is first written.
    async fn files(&self) -> Vec<InstructionFile> {
        let targets: Vec<LearningTarget> = self.deps.targets.clone();
        let root = self.root.clone();
        let config_dir = self.deps.config_dir.clone();
        smol::unblock(move || {
            targets
                .into_iter()
                .filter_map(|target| {
                    let path = targets::target_path(target, &root, config_dir.as_deref())?;
                    InstructionFile::read(target, path)
                        .map_err(|error| warn!(%error, target = target.as_str(), "instruction file unreadable"))
                        .ok()
                })
                .collect()
        })
        .await
    }

    fn announce(&self, run_id: i64, step: LearnerStep) {
        self.deps
            .spend
            .events
            .try_send(AgentEvent::LearnerActivity(Box::new(LearnerActivity {
                run_id,
                step,
            })));
    }

    async fn store(&self) -> Result<LearningStore, LearnError> {
        let state = self.deps.state.clone();
        Ok(smol::unblock(move || LearningStore::open(&state)).await?)
    }
}

fn notice(run_id: i64, finished: &FinishedRun) -> LearnedNotice {
    let count = |target: LearningTarget| {
        finished
            .edits
            .iter()
            .filter(|edit| edit.target == target.as_str())
            .count()
    };
    LearnedNotice {
        run_id,
        summary: finished.summary.clone(),
        notes: count(LearningTarget::Memory),
        project: count(LearningTarget::Project),
        local: count(LearningTarget::Local),
        global: count(LearningTarget::Global),
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn unsigned(ms: i64) -> u64 {
    u64::try_from(ms).unwrap_or_default()
}

fn signed(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const NOW_MS: i64 = 1_767_225_600_000;

    fn last(outcome: &str, ago: Duration) -> LearningRun {
        LearningRun {
            id: 1,
            started_ms: NOW_MS - millis(ago),
            finished_ms: None,
            from_ms: 0,
            through_ms: 0,
            outcome: outcome.to_owned(),
            model: String::new(),
            tokens: 0,
            summary: String::new(),
            dropped: String::new(),
        }
    }

    #[test_case(None, 0, false, false => None ; "a_first_run")]
    #[test_case(Some((LEARNED, Duration::from_secs(60))), 1, false, false => Some(TOO_SOON) ; "right_after_a_run")]
    #[test_case(Some((LEARNED, Duration::from_secs(60))), 1, true, false => None ; "forced_right_after_a_run")]
    #[test_case(Some((LEARNED, MIN_INTERVAL)), 1, false, false => None ; "once_the_interval_passed")]
    #[test_case(Some((PARTIAL, BACKLOG_INTERVAL)), 1, false, false => None ; "a_backlog_sooner")]
    #[test_case(Some((FAILED, BACKLOG_INTERVAL)), 1, false, false => Some(TOO_SOON) ; "a_failure_waits_the_full_interval")]
    #[test_case(Some((LEARNED, MIN_INTERVAL)), MAX_RUNS_PER_DAY, true, false => Some(DAILY_CAP) ; "the_daily_cap_holds_when_forced")]
    #[test_case(Some((LEARNED, BACKLOG_INTERVAL)), 1, false, true => None ; "empty_memory_sooner")]
    #[test_case(Some((LEARNED, BACKLOG_INTERVAL)), EMPTY_MEMORY_RUNS_PER_DAY, false, true => Some(TOO_SOON) ; "empty_memory_pace_runs_out_for_the_day")]
    #[test_case(Some((LEARNED, MIN_INTERVAL)), MAX_RUNS_PER_DAY, false, true => Some(DAILY_CAP) ; "empty_memory_keeps_the_daily_cap")]
    fn when_a_learner_runs(
        previous: Option<(&str, Duration)>,
        runs_today: usize,
        forced: bool,
        empty_memory: bool,
    ) -> Option<&'static str> {
        let last = previous.map(|(outcome, ago)| last(outcome, ago));
        waiting(
            None,
            last.as_ref(),
            runs_today,
            NOW_MS,
            forced,
            empty_memory,
        )
    }

    #[test_case(false, 0 => MIN_USER_MESSAGES ; "notes_exist")]
    #[test_case(true, 0 => EMPTY_MEMORY_USER_MESSAGES ; "no_notes")]
    #[test_case(true, EMPTY_MEMORY_RUNS_PER_DAY => MIN_USER_MESSAGES ; "no_notes_after_the_early_runs")]
    fn how_many_messages_a_run_waits_for(empty_memory: bool, runs_today: usize) -> usize {
        min_user_messages(empty_memory, runs_today)
    }

    #[test]
    fn a_paused_project_waits_even_when_forced() {
        let paused = LearningProject {
            paused: true,
            ..LearningProject::default()
        };
        assert_eq!(
            waiting(Some(&paused), None, 0, NOW_MS, true, true),
            Some(PAUSED)
        );
    }
}
