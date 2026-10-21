//! What a PR's GitHub state means: snapshot reads, the blocker tiers, the single and stack polling
//! loop, and the queued-stack state machine that follows a captured stack until it merges.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

use super::github::{resolve_checks, resolve_review_threads};
use super::types::{
    Blocker, Check, CheckKind, CiKind, CiState, DraftGate, Event, GitHubMergeAssessment,
    GitHubReader, MergeBasis, MergeGateReason, MergeStateStatus, Mergeability, MergedPr,
    PollingOptions, PrContext, PrNumber, PrSnapshot, PrState, PullRequestFacts, QueryFailure,
    ReadyGate, ReadyPr, ReadyProof, ReadyScope, ReviewDecision, RollupState, Settled, StatusReason,
    TimeoutReason, Verdict, WaitReason, WatchError, WatchMode,
};
use crate::host::{Cancelled, Host};

const AUTOMATION_TOKENS: [&str; 4] = [
    "bugbot",
    "security review",
    "pr review automation",
    "review automation",
];
const QUERY_BACKOFF_FLOOR_SECONDS: f64 = 60.0;
const QUERY_BACKOFF_CAP_SECONDS: f64 = 300.0;
const OBSERVED_AT_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";
const NO_DECISION: &str = "snapshot has no classified decision";
const STACK_NO_DECISION: &str = "stack has no classified decision";
const QUEUED_NEEDS_STATUS_ONLY: &str = "queued-stack requires status-only in the simple runner";
const NO_READ_IN_FLIGHT: &str = "queue has no read in flight";
const SWEEP_HEAD_MISMATCH: &str = "snapshot does not match sweep head";
const SWEEP_INCOMPLETE: &str = "sweep completed without every snapshot";
const EMPTY_QUEUE_COMPLETE: &str = "empty queue cannot complete";
const NO_WORK_WHILE_ACTIVE: &str = "queue has no work while active";
const BLOCKER_TIERS: [fn(&PrSnapshot) -> Option<Blocker>; 3] =
    [conflict_blocker, thread_blocker, ci_blocker];

fn fatal<T>(message: &str) -> Result<T, WatchError> {
    Err(WatchError::Fatal(message.to_owned()))
}

/// Seconds on the host's clock, which only differences are taken of.
pub fn clock_now(host: &dyn Host) -> f64 {
    host.now().as_duration().as_secs_f64()
}

fn observed_at(host: &dyn Host) -> String {
    host.now().strftime(OBSERVED_AT_FORMAT).to_string()
}

fn sleep_seconds(host: &dyn Host, seconds: f64) -> Result<(), Cancelled> {
    host.sleep(Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX))
}

pub fn assess_github_merge(
    merge_state_status: MergeStateStatus,
    head_rollup_state: Option<RollupState>,
) -> GitHubMergeAssessment {
    match (merge_state_status, head_rollup_state) {
        (MergeStateStatus::Blocked, Some(RollupState::Error | RollupState::Failure)) => {
            GitHubMergeAssessment::Refused {
                merge_state_status,
                head_rollup_state,
            }
        }
        (MergeStateStatus::Blocked, _) => GitHubMergeAssessment::Allowed {
            basis: MergeBasis::Rollup,
            merge_state_status,
            head_rollup_state,
        },
        _ => GitHubMergeAssessment::Allowed {
            basis: MergeBasis::MergeState,
            merge_state_status,
            head_rollup_state,
        },
    }
}

fn merge_assessment(
    reader: &dyn GitHubReader,
    facts: &PullRequestFacts,
) -> Result<(bool, GitHubMergeAssessment), WatchError> {
    let commits = reader.commit_rollups(&facts.context)?;
    let head = facts.head_ref_oid.as_deref();
    let head_rollup_state = head
        .and_then(|oid| commits.iter().find(|commit| commit.oid == oid))
        .and_then(|commit| commit.state);
    let had_previous_passing_ci = commits.iter().any(|commit| {
        Some(commit.oid.as_str()) != head && commit.state == Some(RollupState::Success)
    });
    Ok((
        had_previous_passing_ci,
        assess_github_merge(facts.merge_state_status, head_rollup_state),
    ))
}

/// Whether a snapshot whose checks are only pending still reads commit history. The queued
/// watcher skips that query while it waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingHistory {
    Include,
    Omit,
}

fn checks_of(checks: &[Check], kind: CheckKind) -> Vec<Check> {
    checks
        .iter()
        .filter(|check| check.kind == kind)
        .cloned()
        .collect()
}

pub fn read_snapshot(
    reader: &dyn GitHubReader,
    context: &PrContext,
    pending_history: PendingHistory,
) -> Result<PrSnapshot, WatchError> {
    let facts = reader.pull_request(context)?;
    let context = context.clone();
    if facts.state == PrState::Merged || facts.merged_at.is_some() {
        return Ok(PrSnapshot::Merged { context, facts });
    }
    if facts.state == PrState::Closed {
        return Ok(PrSnapshot::Closed { context, facts });
    }
    let threads = resolve_review_threads(reader, &context)?;
    let checks = resolve_checks(reader, &context)?;
    let failed = checks_of(&checks.checks, CheckKind::Failed);
    let pending = checks_of(&checks.checks, CheckKind::Pending);
    let review_automation_running = checks.checks.iter().any(|check| {
        let name = check.name.to_lowercase();
        check.kind == CheckKind::Pending
            && AUTOMATION_TOKENS.iter().any(|token| name.contains(token))
    });
    let ci = if failed.is_empty() && !pending.is_empty() && pending_history == PendingHistory::Omit
    {
        CiState {
            source: checks.source,
            all: checks.checks,
            had_previous_passing_ci: false,
            kind: CiKind::Pending,
            failed,
            pending,
            github: None,
            history_omitted: true,
        }
    } else {
        let (had_previous_passing_ci, github) = merge_assessment(reader, &facts)?;
        let refused = matches!(github, GitHubMergeAssessment::Refused { .. });
        let (kind, github) = if !failed.is_empty() {
            (CiKind::Failing, Some(github))
        } else if refused {
            (CiKind::GithubRejected, Some(github))
        } else if !pending.is_empty() {
            (CiKind::Pending, None)
        } else {
            (CiKind::Clean, Some(github))
        };
        CiState {
            source: checks.source,
            all: checks.checks,
            had_previous_passing_ci,
            kind,
            failed,
            pending,
            github,
            history_omitted: false,
        }
    };
    Ok(PrSnapshot::Open {
        context,
        facts,
        threads,
        ci,
        review_automation_running,
    })
}

fn conflict_blocker(row: &PrSnapshot) -> Option<Blocker> {
    match row {
        PrSnapshot::Open { context, facts, .. } if facts.has_conflicts() => {
            Some(Blocker::MergeConflicts {
                pr: context.clone(),
                facts: facts.clone(),
            })
        }
        _ => None,
    }
}

fn thread_blocker(row: &PrSnapshot) -> Option<Blocker> {
    match row {
        PrSnapshot::Open {
            context, threads, ..
        } if !threads.is_empty() => Some(Blocker::ReviewThreads {
            pr: context.clone(),
            threads: threads.clone(),
        }),
        _ => None,
    }
}

fn ci_blocker(row: &PrSnapshot) -> Option<Blocker> {
    match row {
        PrSnapshot::Open { context, ci, .. }
            if matches!(ci.kind, CiKind::Failing | CiKind::GithubRejected) =>
        {
            Some(Blocker::FailingChecks {
                pr: context.clone(),
                ci: ci.clone(),
            })
        }
        _ => None,
    }
}

fn gate_reason(row: &PrSnapshot, allow_draft: bool) -> Option<MergeGateReason> {
    match row {
        PrSnapshot::Merged { .. } => None,
        PrSnapshot::Closed { .. } => Some(MergeGateReason::ClosedWithoutMerge),
        PrSnapshot::Open { facts, .. } if facts.is_draft && !allow_draft => {
            Some(MergeGateReason::DraftPr)
        }
        PrSnapshot::Open { facts, .. } => (facts.review_decision
            == Some(ReviewDecision::ChangesRequested))
        .then_some(MergeGateReason::ChangesRequested),
    }
}

/// A draft whose checks still run is a wait, not a blocker: the gate is reported once CI settles.
fn gate_blocker(row: &PrSnapshot, allow_draft: bool) -> Option<Blocker> {
    let reason = gate_reason(row, allow_draft)?;
    let draft_waiting = reason == MergeGateReason::DraftPr && row.pending_ci().is_some();
    (!draft_waiting).then(|| Blocker::MergeGate {
        pr: row.context().clone(),
        reason,
    })
}

fn ready_contribution(row: &PrSnapshot, allow_draft: bool) -> Option<Settled> {
    match row {
        PrSnapshot::Merged { context, facts } => Some(Settled::Merged(MergedPr {
            context: context.clone(),
            merged_at: facts.merged_at.clone(),
        })),
        PrSnapshot::Open {
            context,
            facts,
            threads,
            ci,
            ..
        } if ci.kind == CiKind::Clean
            && threads.is_empty()
            && !facts.has_conflicts()
            && gate_reason(row, allow_draft).is_none() =>
        {
            Some(Settled::Ready(ReadyPr {
                context: context.clone(),
                proof: ReadyProof {
                    mergeability: Mergeability::Clear,
                    threads: [],
                    ci: ci.clone(),
                    gate: ReadyGate {
                        state: PrState::Open,
                        review_decision: facts.review_decision,
                        draft: if facts.is_draft {
                            DraftGate::DraftAllowed
                        } else {
                            DraftGate::NotDraft
                        },
                    },
                },
            }))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrDecision {
    Blocker(Blocker),
    Waiting {
        frontier: PrContext,
        pending: Vec<Check>,
    },
    Settled(Settled),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StackDecision {
    Blocker(Blocker),
    Waiting {
        frontier: PrContext,
        pending: Vec<Check>,
    },
    Clear(Vec<Settled>),
}

fn waiting(row: &PrSnapshot) -> Option<(PrContext, Vec<Check>)> {
    row.pending_ci()
        .map(|pending| (row.context().clone(), pending.to_vec()))
}

pub fn classify_pr(row: &PrSnapshot, allow_draft: bool) -> Result<PrDecision, WatchError> {
    let blocker = conflict_blocker(row)
        .or_else(|| thread_blocker(row))
        .or_else(|| ci_blocker(row))
        .or_else(|| gate_blocker(row, allow_draft));
    if let Some(blocker) = blocker {
        return Ok(PrDecision::Blocker(blocker));
    }
    if let Some((frontier, pending)) = waiting(row) {
        return Ok(PrDecision::Waiting { frontier, pending });
    }
    ready_contribution(row, allow_draft)
        .map(PrDecision::Settled)
        .map_or_else(|| fatal(NO_DECISION), Ok)
}

/// Scans every row tier by tier, so an upstack conflict outranks a failing check at the bottom.
pub fn select_tier_major_stack_decision(
    rows: &[PrSnapshot],
    allow_draft: bool,
) -> Result<StackDecision, WatchError> {
    let blocker = BLOCKER_TIERS
        .iter()
        .find_map(|tier| rows.iter().find_map(tier))
        .or_else(|| rows.iter().find_map(|row| gate_blocker(row, allow_draft)));
    if let Some(blocker) = blocker {
        return Ok(StackDecision::Blocker(blocker));
    }
    if let Some((frontier, pending)) = rows.iter().find_map(waiting) {
        return Ok(StackDecision::Waiting { frontier, pending });
    }
    let prs: Vec<Settled> = rows
        .iter()
        .filter_map(|row| ready_contribution(row, allow_draft))
        .collect();
    if prs.is_empty() || prs.len() != rows.len() {
        return fatal(STACK_NO_DECISION);
    }
    Ok(StackDecision::Clear(prs))
}

pub fn query_backoff_seconds(interval: f64, failures: u64) -> f64 {
    let exponent = i32::try_from(failures.saturating_sub(1)).unwrap_or(i32::MAX);
    (interval.max(QUERY_BACKOFF_FLOOR_SECONDS) * 2f64.powi(exponent)).min(QUERY_BACKOFF_CAP_SECONDS)
}

/// Stamps each verdict of one run with the next sequence number and the time it was observed.
pub struct VerdictFactory<'a> {
    host: &'a dyn Host,
    mode: WatchMode,
    sequence: Cell<u64>,
}

impl<'a> VerdictFactory<'a> {
    pub fn new(host: &'a dyn Host, mode: WatchMode) -> Self {
        Self {
            host,
            mode,
            sequence: Cell::new(0),
        }
    }

    pub fn stamp(&self, event: Event) -> Verdict {
        self.sequence.set(self.sequence.get() + 1);
        Verdict {
            sequence: self.sequence.get(),
            observed_at: observed_at(self.host),
            mode: self.mode,
            event,
        }
    }
}

pub fn status_query_verdict(
    stamp: &VerdictFactory,
    failures: u64,
    failure: QueryFailure,
) -> Verdict {
    stamp.stamp(Event::Blocker(Blocker::StatusQuery { failures, failure }))
}

pub struct RunDependencies<'a> {
    pub reader: &'a dyn GitHubReader,
    pub host: &'a dyn Host,
    pub emit: &'a mut dyn FnMut(&Verdict),
}

fn deadline_passed(started: f64, options: &PollingOptions, now: f64) -> bool {
    options.timeout > 0.0 && now - started >= options.timeout
}

enum StepResult {
    Terminal(Verdict),
    Sleep {
        seconds: f64,
        on_deadline: Option<Event>,
    },
    Continue,
}

/// Runs `step` until it returns a verdict, retrying a failed GitHub read with backoff until the
/// error budget or the deadline runs out.
fn poll_until_terminal(
    dependencies: &mut RunDependencies,
    options: &PollingOptions,
    stamp: &VerdictFactory,
    mut step: impl FnMut(&mut RunDependencies) -> Result<StepResult, WatchError>,
) -> Result<Verdict, WatchError> {
    let mut failures = 0;
    let started = clock_now(dependencies.host);
    loop {
        let result = match step(dependencies) {
            Ok(result) => {
                failures = 0;
                result
            }
            Err(WatchError::Query(failure)) => {
                failures += 1;
                if !failure.retryable() || failures >= options.max_query_errors {
                    return Ok(status_query_verdict(stamp, failures, failure));
                }
                let retry_in_seconds = query_backoff_seconds(options.interval, failures);
                (dependencies.emit)(&stamp.stamp(Event::Retry {
                    failure: failure.clone(),
                    consecutive_failures: failures,
                    retry_in_seconds,
                }));
                if deadline_passed(started, options, clock_now(dependencies.host)) {
                    return Ok(
                        stamp.stamp(Event::Timeout(TimeoutReason::StatusUnavailable { failure }))
                    );
                }
                sleep_seconds(dependencies.host, retry_in_seconds)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        match result {
            StepResult::Terminal(verdict) => return Ok(verdict),
            StepResult::Sleep {
                seconds,
                on_deadline,
            } => {
                if let Some(event) = on_deadline
                    && deadline_passed(started, options, clock_now(dependencies.host))
                {
                    return Ok(stamp.stamp(event));
                }
                sleep_seconds(dependencies.host, seconds)?;
            }
            StepResult::Continue => {}
        }
    }
}

pub fn run_simple(
    dependencies: &mut RunDependencies,
    contexts: &[PrContext],
    mode: WatchMode,
    status_only: bool,
    options: &PollingOptions,
) -> Result<Verdict, WatchError> {
    let stamp = VerdictFactory::new(dependencies.host, mode);
    let step = |dependencies: &mut RunDependencies| {
        let rows = contexts
            .iter()
            .map(|context| read_snapshot(dependencies.reader, context, PendingHistory::Include))
            .collect::<Result<Vec<_>, _>>()?;
        if status_only {
            return Ok(StepResult::Terminal(stamp.stamp(Event::Status {
                reason: StatusReason::StatusOnly,
                rows,
            })));
        }
        let decision = match mode {
            WatchMode::QueuedStack => return fatal(QUEUED_NEEDS_STATUS_ONLY),
            WatchMode::Single => match rows.first() {
                Some(row) => match classify_pr(row, options.allow_draft)? {
                    PrDecision::Blocker(blocker) => StackDecision::Blocker(blocker),
                    PrDecision::Waiting { frontier, pending } => {
                        StackDecision::Waiting { frontier, pending }
                    }
                    PrDecision::Settled(pr) => {
                        return Ok(StepResult::Terminal(
                            stamp.stamp(Event::Ready(ReadyScope::Single { pr })),
                        ));
                    }
                },
                None => return fatal(NO_DECISION),
            },
            WatchMode::Stack => {
                let decision = select_tier_major_stack_decision(&rows, options.allow_draft)?;
                (dependencies.emit)(&stamp.stamp(Event::Status {
                    reason: StatusReason::Poll,
                    rows,
                }));
                decision
            }
        };
        match decision {
            StackDecision::Blocker(blocker) => {
                Ok(StepResult::Terminal(stamp.stamp(Event::Blocker(blocker))))
            }
            StackDecision::Clear(prs) => Ok(StepResult::Terminal(
                stamp.stamp(Event::Ready(ReadyScope::Stack { prs })),
            )),
            StackDecision::Waiting { frontier, pending } => {
                (dependencies.emit)(&stamp.stamp(Event::Waiting {
                    frontier,
                    reason: WaitReason::PendingChecks {
                        pending: pending.clone(),
                    },
                }));
                Ok(StepResult::Sleep {
                    seconds: options.interval,
                    on_deadline: Some(Event::Timeout(TimeoutReason::PendingChecks { pending })),
                })
            }
        }
    };
    poll_until_terminal(dependencies, options, &stamp, step)
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueueWork {
    WholeStackSweep { remaining: Vec<PrContext> },
    FrontierPoll { frontier: PrContext },
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueState {
    pub queue: Vec<PrContext>,
    pub snapshots: HashMap<PrNumber, PrSnapshot>,
    pub work: Option<QueueWork>,
    pub next_sweep_at: f64,
    pub frontier: Option<PrContext>,
    pub last_wait_key: Option<String>,
    pub started_at: f64,
}

impl QueueState {
    pub fn new(queue: Vec<PrContext>, now: f64) -> Self {
        Self {
            work: Some(QueueWork::WholeStackSweep {
                remaining: queue.clone(),
            }),
            queue,
            snapshots: HashMap::new(),
            next_sweep_at: now,
            frontier: None,
            last_wait_key: None,
            started_at: now,
        }
    }

    fn ordered_rows(&self) -> Vec<&PrSnapshot> {
        self.queue
            .iter()
            .filter_map(|context| self.snapshots.get(&context.number))
            .collect()
    }

    fn active_rows(&self) -> Vec<&PrSnapshot> {
        self.ordered_rows()
            .into_iter()
            .filter(|row| !row.is_merged())
            .collect()
    }
}

/// Picks the next read: a whole-stack sweep when one is due, else a poll of the frontier.
pub fn plan_queue(state: &mut QueueState, now: f64) {
    if state.work.is_some() {
        return;
    }
    if state.snapshots.is_empty() || now >= state.next_sweep_at {
        let remaining: Vec<PrContext> = state
            .queue
            .iter()
            .filter(|context| {
                !state
                    .snapshots
                    .get(&context.number)
                    .is_some_and(PrSnapshot::is_merged)
            })
            .cloned()
            .collect();
        if !remaining.is_empty() {
            state.work = Some(QueueWork::WholeStackSweep { remaining });
            return;
        }
    }
    state.work = state
        .active_rows()
        .first()
        .map(|row| QueueWork::FrontierPoll {
            frontier: row.context().clone(),
        });
}

/// Records a successful read and returns the sweep's rows when it completed the sweep. A sweep
/// head drops only once its snapshot is in, so a failed read resumes at the same PR.
pub fn apply_queue_snapshot(
    state: &mut QueueState,
    snapshot: PrSnapshot,
    now: f64,
    options: &PollingOptions,
) -> Result<Option<Vec<PrSnapshot>>, WatchError> {
    let Some(work) = state.work.take() else {
        return fatal(NO_READ_IN_FLIGHT);
    };
    let number = snapshot.context().number;
    state.snapshots.insert(number, snapshot);
    let QueueWork::WholeStackSweep { remaining } = work else {
        return Ok(None);
    };
    let Some((head, tail)) = remaining.split_first() else {
        return fatal(SWEEP_HEAD_MISMATCH);
    };
    if head.number != number {
        return fatal(SWEEP_HEAD_MISMATCH);
    }
    if !tail.is_empty() {
        state.work = Some(QueueWork::WholeStackSweep {
            remaining: tail.to_vec(),
        });
        return Ok(None);
    }
    let rows: Vec<PrSnapshot> = state.ordered_rows().into_iter().cloned().collect();
    if rows.len() != state.queue.len() {
        return fatal(SWEEP_INCOMPLETE);
    }
    state.next_sweep_at = now + options.sweep_interval;
    Ok(Some(rows))
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueueEvaluation {
    Complete {
        merged: Vec<MergedPr>,
    },
    Blocker(Blocker),
    Advance {
        merged: PrContext,
        frontier: PrContext,
        remaining: usize,
    },
    Timeout {
        frontier: PrContext,
        unmerged_count: usize,
    },
    Waiting {
        frontier: PrContext,
        reason: WaitReason,
        emit: bool,
    },
}

/// Queued mode reports its own merge frontier: once that PR is blocker-free it waits on the merge
/// queue, ignoring upstack pending checks, because those do not block the frontier's merge.
pub fn evaluate_queue(
    state: &mut QueueState,
    now: f64,
    options: &PollingOptions,
) -> Result<QueueEvaluation, WatchError> {
    let active: Vec<PrSnapshot> = state.active_rows().into_iter().cloned().collect();
    let Some(row) = active.first() else {
        let merged: Vec<MergedPr> = state
            .ordered_rows()
            .into_iter()
            .filter_map(|row| match row {
                PrSnapshot::Merged { context, facts } => Some(MergedPr {
                    context: context.clone(),
                    merged_at: facts.merged_at.clone(),
                }),
                _ => None,
            })
            .collect();
        if merged.is_empty() {
            return fatal(EMPTY_QUEUE_COMPLETE);
        }
        return Ok(QueueEvaluation::Complete { merged });
    };
    if let StackDecision::Blocker(blocker) =
        select_tier_major_stack_decision(&active, options.allow_draft)?
    {
        return Ok(QueueEvaluation::Blocker(blocker));
    }
    let frontier = row.context().clone();
    if let Some(previous) = &state.frontier
        && previous.number != frontier.number
    {
        let merged = previous.clone();
        state.frontier = Some(frontier.clone());
        state.last_wait_key = None;
        return Ok(QueueEvaluation::Advance {
            merged,
            frontier,
            remaining: active.len(),
        });
    }
    state.frontier = Some(frontier.clone());
    if deadline_passed(state.started_at, options, now) {
        return Ok(QueueEvaluation::Timeout {
            frontier,
            unmerged_count: active.len(),
        });
    }
    let (reason, key) = match row.pending_ci() {
        Some(pending) => (
            WaitReason::PendingChecks {
                pending: pending.to_vec(),
            },
            format!("pending:{}:{}", frontier.number, pending.len()),
        ),
        None => (
            WaitReason::MergeQueue {
                unmerged_count: active.len(),
            },
            format!("queue:{}:{}", frontier.number, active.len()),
        ),
    };
    let emit = state.last_wait_key.as_ref() != Some(&key);
    state.last_wait_key = Some(key);
    Ok(QueueEvaluation::Waiting {
        frontier,
        reason,
        emit,
    })
}

pub fn run_queued(
    dependencies: &mut RunDependencies,
    contexts: Vec<PrContext>,
    options: &PollingOptions,
) -> Result<Verdict, WatchError> {
    let mut state = QueueState::new(contexts.clone(), clock_now(dependencies.host));
    let stamp = VerdictFactory::new(dependencies.host, WatchMode::QueuedStack);
    (dependencies.emit)(&stamp.stamp(Event::Queue { queue: contexts }));
    let complete = |state: &QueueState, merged| {
        StepResult::Terminal(stamp.stamp(Event::Complete {
            queue: state.queue.clone(),
            merged,
        }))
    };
    let step = |dependencies: &mut RunDependencies| {
        plan_queue(&mut state, clock_now(dependencies.host));
        let context = match &state.work {
            None => {
                return match evaluate_queue(&mut state, clock_now(dependencies.host), options)? {
                    QueueEvaluation::Complete { merged } => Ok(complete(&state, merged)),
                    _ => fatal(NO_WORK_WHILE_ACTIVE),
                };
            }
            Some(QueueWork::WholeStackSweep { remaining }) => match remaining.first() {
                Some(head) => head.clone(),
                None => return fatal(SWEEP_HEAD_MISMATCH),
            },
            Some(QueueWork::FrontierPoll { frontier }) => frontier.clone(),
        };
        let snapshot = read_snapshot(dependencies.reader, &context, PendingHistory::Omit)?;
        let now = clock_now(dependencies.host);
        if let Some(rows) = apply_queue_snapshot(&mut state, snapshot, now, options)? {
            (dependencies.emit)(&stamp.stamp(Event::Status {
                reason: StatusReason::WholeStackSweep,
                rows,
            }));
        }
        if state.work.is_some() {
            return Ok(StepResult::Continue);
        }
        Ok(
            match evaluate_queue(&mut state, clock_now(dependencies.host), options)? {
                QueueEvaluation::Complete { merged } => complete(&state, merged),
                QueueEvaluation::Blocker(blocker) => {
                    StepResult::Terminal(stamp.stamp(Event::Blocker(blocker)))
                }
                QueueEvaluation::Advance {
                    merged,
                    frontier,
                    remaining,
                } => {
                    (dependencies.emit)(&stamp.stamp(Event::Advance {
                        merged,
                        frontier,
                        remaining,
                    }));
                    StepResult::Continue
                }
                QueueEvaluation::Timeout {
                    frontier,
                    unmerged_count,
                } => {
                    StepResult::Terminal(stamp.stamp(Event::Timeout(TimeoutReason::QueuedStack {
                        frontier,
                        unmerged_count,
                    })))
                }
                QueueEvaluation::Waiting {
                    frontier,
                    reason,
                    emit,
                } => {
                    if emit {
                        (dependencies.emit)(&stamp.stamp(Event::Waiting { frontier, reason }));
                    }
                    StepResult::Sleep {
                        seconds: options.interval,
                        on_deadline: None,
                    }
                }
            },
        )
    };
    poll_until_terminal(dependencies, options, &stamp, step)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;

    use test_case::test_case;

    use super::super::fakes::{
        CI, FakeReader, HEAD, SLEEP, Timeline, TimelineHost, context, default_facts, entries,
        failed_check, fake_host, passing_check, pending_check, push,
    };
    use super::super::types::{ChecksFastPath, CommitRollup, MergeableState};
    use super::*;

    const MERGED_AT: &str = "2026-07-26T00:00:00Z";
    const UPSTACK_BUILD: &str = "upstack-build";
    const RATE_LIMITED: &str = "rate limited";

    fn options() -> PollingOptions {
        PollingOptions {
            interval: 10.0,
            sweep_interval: 300.0,
            timeout: 0.0,
            max_query_errors: 5,
            allow_draft: false,
        }
    }

    fn snapshot(reader: &FakeReader, number: PrNumber, history: PendingHistory) -> PrSnapshot {
        read_snapshot(reader, &context(number), history).expect("snapshot")
    }

    fn checks(checks: Vec<Check>) -> ChecksFastPath {
        ChecksFastPath::Checks(checks)
    }

    fn ci_kind(row: &PrSnapshot) -> Option<CiKind> {
        match row {
            PrSnapshot::Open { ci, .. } => Some(ci.kind),
            _ => None,
        }
    }

    fn blocked_with_head(state: RollupState) -> FakeReader {
        FakeReader {
            facts: PullRequestFacts {
                merge_state_status: MergeStateStatus::Blocked,
                ..default_facts()
            },
            fast_path: checks(vec![passing_check(CI)]),
            commit_rollups: vec![CommitRollup {
                oid: HEAD.to_owned(),
                state: Some(state),
            }],
            ..FakeReader::default()
        }
    }

    #[test_case(MergeStateStatus::Blocked, Some(RollupState::Failure), true ; "blocked_failure")]
    #[test_case(MergeStateStatus::Blocked, Some(RollupState::Error), true ; "blocked_error")]
    #[test_case(MergeStateStatus::Blocked, Some(RollupState::Pending), false ; "blocked_pending")]
    #[test_case(MergeStateStatus::Unstable, Some(RollupState::Failure), false ; "unstable_failure")]
    #[test_case(MergeStateStatus::Unknown, Some(RollupState::Error), false ; "unknown_error")]
    #[test_case(MergeStateStatus::Unknown, Some(RollupState::Expected), false ; "unknown_expected")]
    #[test_case(MergeStateStatus::Unknown, Some(RollupState::Failure), false ; "unknown_failure")]
    #[test_case(MergeStateStatus::Unknown, Some(RollupState::Pending), false ; "unknown_pending")]
    #[test_case(MergeStateStatus::Unknown, Some(RollupState::Success), false ; "unknown_success")]
    #[test_case(MergeStateStatus::Unknown, None, false ; "unknown_null")]
    #[test_case(MergeStateStatus::Clean, Some(RollupState::Success), false ; "clean_success")]
    fn covers_the_readiness_truth_table(
        merge_state_status: MergeStateStatus,
        head_rollup_state: Option<RollupState>,
        refused: bool,
    ) {
        let assessment = assess_github_merge(merge_state_status, head_rollup_state);
        assert_eq!(
            matches!(assessment, GitHubMergeAssessment::Refused { .. }),
            refused
        );
    }

    #[test]
    fn turns_a_clean_visible_list_plus_github_refusal_into_an_explicit_ci_blocker() {
        let row = snapshot(
            &blocked_with_head(RollupState::Failure),
            1,
            PendingHistory::Include,
        );
        assert_eq!(ci_kind(&row), Some(CiKind::GithubRejected));
        assert!(matches!(
            classify_pr(&row, false).expect("decision"),
            PrDecision::Blocker(Blocker::FailingChecks { .. })
        ));
    }

    #[test]
    fn does_not_query_commit_rollups_while_queued_checks_are_pending() {
        let reader = FakeReader {
            fast_path: checks(vec![pending_check(CI)]),
            ..FakeReader::default()
        };
        let row = snapshot(&reader, 2, PendingHistory::Omit);
        assert_eq!(ci_kind(&row), Some(CiKind::Pending));
        assert_eq!(
            reader.calls(),
            ["pullRequest", "reviewThreadPage:null", "checksFastPath"]
        );
    }

    #[test_case(passing_check(CI) ; "settled")]
    #[test_case(failed_check(CI) ; "failed")]
    fn queries_rollups_for_settled_and_failed_lists(listed: Check) {
        let reader = FakeReader {
            fast_path: checks(vec![listed]),
            ..FakeReader::default()
        };
        snapshot(&reader, 3, PendingHistory::Omit);
        assert!(reader.calls().contains(&"commitRollups".to_owned()));
    }

    #[test]
    fn short_circuits_merged_rows_before_threads_and_checks() {
        let reader = FakeReader {
            facts: PullRequestFacts {
                state: PrState::Merged,
                merged_at: Some(MERGED_AT.to_owned()),
                ..default_facts()
            },
            ..FakeReader::default()
        };
        assert!(snapshot(&reader, 5, PendingHistory::Include).is_merged());
        assert_eq!(reader.calls(), ["pullRequest"]);
    }

    #[test]
    fn scans_stacks_tier_major_so_an_upstack_conflict_outranks_frontier_ci() {
        let frontier = snapshot(
            &FakeReader {
                fast_path: checks(vec![failed_check(CI)]),
                commit_rollups: vec![CommitRollup {
                    oid: HEAD.to_owned(),
                    state: Some(RollupState::Failure),
                }],
                ..FakeReader::default()
            },
            10,
            PendingHistory::Omit,
        );
        let upstack = snapshot(
            &FakeReader {
                facts: PullRequestFacts {
                    mergeable: MergeableState::Conflicting,
                    ..default_facts()
                },
                ..FakeReader::default()
            },
            11,
            PendingHistory::Omit,
        );
        let decision =
            select_tier_major_stack_decision(&[frontier, upstack], false).expect("decision");
        assert!(matches!(
            decision,
            StackDecision::Blocker(Blocker::MergeConflicts { ref pr, .. }) if pr.number == 11
        ));
    }

    #[test]
    fn attributes_a_stack_wait_to_the_pr_whose_checks_are_pending_not_the_bottom() {
        let ready_bottom = snapshot(&FakeReader::default(), 20, PendingHistory::Omit);
        let pending_upstack = snapshot(
            &FakeReader {
                fast_path: checks(vec![pending_check(UPSTACK_BUILD)]),
                ..FakeReader::default()
            },
            21,
            PendingHistory::Omit,
        );
        let decision = select_tier_major_stack_decision(&[ready_bottom, pending_upstack], false)
            .expect("decision");
        let StackDecision::Waiting { frontier, pending } = decision else {
            panic!("expected a wait, got {decision:?}");
        };
        assert_eq!(frontier.number, 21);
        assert_eq!(pending, [pending_check(UPSTACK_BUILD)]);
    }

    #[test]
    fn waits_on_a_draft_while_checks_are_pending_then_reports_the_draft_gate() {
        let draft = PullRequestFacts {
            is_draft: true,
            ..default_facts()
        };
        let pending = snapshot(
            &FakeReader {
                facts: draft.clone(),
                fast_path: checks(vec![pending_check(CI)]),
                ..FakeReader::default()
            },
            12,
            PendingHistory::Omit,
        );
        assert!(matches!(
            classify_pr(&pending, false).expect("decision"),
            PrDecision::Waiting { .. }
        ));
        let settled = snapshot(
            &FakeReader {
                facts: draft,
                ..FakeReader::default()
            },
            12,
            PendingHistory::Omit,
        );
        assert!(matches!(
            classify_pr(&settled, false).expect("decision"),
            PrDecision::Blocker(Blocker::MergeGate {
                reason: MergeGateReason::DraftPr,
                ..
            })
        ));
    }

    fn open_snapshot(number: PrNumber) -> PrSnapshot {
        snapshot(&FakeReader::default(), number, PendingHistory::Omit)
    }

    fn remaining_numbers(state: &QueueState) -> Vec<PrNumber> {
        match &state.work {
            Some(QueueWork::WholeStackSweep { remaining }) => {
                remaining.iter().map(|context| context.number).collect()
            }
            _ => Vec::new(),
        }
    }

    #[test]
    fn drops_a_sweep_head_only_after_its_snapshot_succeeds() {
        let mut state = QueueState::new(vec![context(20), context(21), context(22)], 0.0);
        apply_queue_snapshot(&mut state, open_snapshot(20), 0.0, &options()).expect("applied");
        assert_eq!(remaining_numbers(&state), [21, 22]);
        apply_queue_snapshot(&mut state, open_snapshot(21), 60.0, &options()).expect("applied");
        assert_eq!(remaining_numbers(&state), [22]);
    }

    #[test]
    fn emits_a_completed_sweep_only_after_its_final_successful_snapshot() {
        let mut state = QueueState::new(vec![context(30), context(31)], 0.0);
        let first =
            apply_queue_snapshot(&mut state, open_snapshot(30), 0.0, &options()).expect("first");
        assert_eq!(first, None);
        let second =
            apply_queue_snapshot(&mut state, open_snapshot(31), 5.0, &options()).expect("second");
        let rows: Vec<PrNumber> = second
            .expect("completed sweep")
            .iter()
            .map(|row| row.context().number)
            .collect();
        assert_eq!(rows, [30, 31]);
        assert_eq!(state.next_sweep_at, 305.0);
    }

    #[test]
    fn deduplicates_identical_waits_and_schedules_the_next_due_sweep() {
        let mut state = QueueState::new(vec![context(50)], 0.0);
        apply_queue_snapshot(&mut state, open_snapshot(50), 0.0, &options()).expect("applied");
        let first = evaluate_queue(&mut state, 0.0, &options()).expect("first");
        assert!(matches!(first, QueueEvaluation::Waiting { emit: true, .. }));
        let second = evaluate_queue(&mut state, 10.0, &options()).expect("second");
        assert!(matches!(
            second,
            QueueEvaluation::Waiting { emit: false, .. }
        ));
        plan_queue(&mut state, 300.0);
        assert!(matches!(
            state.work,
            Some(QueueWork::WholeStackSweep { .. })
        ));
    }

    fn run_with_timeline(reader: &FakeReader, timeline: &Timeline, queue: Vec<PrContext>) {
        let host = TimelineHost {
            inner: fake_host(),
            timeline: Arc::clone(timeline),
        };
        host.inner.cancel_after(1);
        let mut emit = |verdict: &Verdict| push(timeline, format!("emit:{}", verdict.event.kind()));
        let mut dependencies = RunDependencies {
            reader,
            host: &host,
            emit: &mut emit,
        };
        let result = run_queued(&mut dependencies, queue, &options());
        assert!(matches!(result, Err(WatchError::Cancelled(_))));
    }

    #[test]
    fn resumes_the_sweep_at_the_pr_whose_read_failed() {
        let timeline = Timeline::default();
        let recorder = Arc::clone(&timeline);
        let fail_next = Cell::new(true);
        let reader = FakeReader {
            on_pull_request: Some(Box::new(move |pr, facts| {
                if pr.number == 21 && fail_next.replace(false) {
                    push(&recorder, format!("fail:{}", pr.number));
                    return Err(WatchError::Query(QueryFailure::CommandExit {
                        code: 1,
                        detail: RATE_LIMITED.to_owned(),
                    }));
                }
                push(&recorder, format!("read:{}", pr.number));
                Ok(facts)
            })),
            ..FakeReader::default()
        };
        run_with_timeline(
            &reader,
            &timeline,
            vec![context(20), context(21), context(22)],
        );
        assert_eq!(
            entries(&timeline),
            [
                "emit:QUEUE",
                "read:20",
                "fail:21",
                "emit:RETRY",
                "sleep",
                "read:21",
                "read:22",
                "emit:STATUS",
                "emit:WAITING",
                "sleep",
            ]
        );
    }

    #[test]
    fn advance_continues_directly_to_the_new_frontier_without_sleeping() {
        let timeline = Timeline::default();
        let recorder = Arc::clone(&timeline);
        let bottom_reads = Cell::new(0);
        let reader = FakeReader {
            on_pull_request: Some(Box::new(move |pr, facts| {
                push(&recorder, format!("read:{}", pr.number));
                if pr.number != 40 {
                    return Ok(facts);
                }
                bottom_reads.set(bottom_reads.get() + 1);
                Ok(if bottom_reads.get() > 1 {
                    PullRequestFacts {
                        state: PrState::Merged,
                        merged_at: Some(MERGED_AT.to_owned()),
                        ..facts
                    }
                } else {
                    facts
                })
            })),
            ..FakeReader::default()
        };
        run_with_timeline(&reader, &timeline, vec![context(40), context(41)]);
        let timeline = entries(&timeline);
        let first_sleep = timeline
            .iter()
            .position(|entry| entry == SLEEP)
            .expect("a sleep");
        assert_eq!(
            timeline[first_sleep..first_sleep + 5],
            [
                "sleep",
                "read:40",
                "emit:ADVANCE",
                "read:41",
                "emit:WAITING"
            ]
        );
    }

    #[test_case(1.0, 1, 60.0 ; "floor")]
    #[test_case(1.0, 2, 120.0 ; "doubles")]
    #[test_case(60.0, 4, 300.0 ; "cap")]
    fn uses_the_specified_retry_floor_and_cap(interval: f64, failures: u64, expected: f64) {
        assert_eq!(query_backoff_seconds(interval, failures), expected);
    }

    #[test]
    fn stamps_verdicts_in_sequence_with_the_observed_time() {
        let host = fake_host();
        let stamp = VerdictFactory::new(&host, WatchMode::Single);
        let first = stamp.stamp(Event::Queue { queue: Vec::new() });
        let second = stamp.stamp(Event::Queue { queue: Vec::new() });
        assert_eq!((first.sequence, second.sequence), (1, 2));
        assert_eq!(first.observed_at, "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn times_out_a_pending_wait_once_the_deadline_passes() {
        let host = fake_host();
        let reader = FakeReader {
            fast_path: checks(vec![pending_check(CI)]),
            ..FakeReader::default()
        };
        let mut kinds = Vec::new();
        let mut emit = |verdict: &Verdict| kinds.push(verdict.event.kind());
        let mut dependencies = RunDependencies {
            reader: &reader,
            host: &host,
            emit: &mut emit,
        };
        let polling = PollingOptions {
            timeout: 15.0,
            ..options()
        };
        let verdict = run_simple(
            &mut dependencies,
            &[context(1)],
            WatchMode::Single,
            false,
            &polling,
        )
        .expect("verdict");
        assert_eq!(verdict.event.exit_code(), Some(5));
        assert_eq!(kinds, ["WAITING", "WAITING", "WAITING"]);
        assert_eq!(
            host.slept(),
            [Duration::from_secs(10), Duration::from_secs(10)]
        );
    }
}
