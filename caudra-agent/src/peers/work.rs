//! The runtime side of consumer groups. A member claims work from the shared
//! history while it idles and offers the item to its next turn as a framed
//! assignment, renewing the lease for as long as it owns the item. A turn
//! that ends without reporting an outcome pauses the item, so no other member
//! repeats its effects unless a person retries it.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use caudra_config::InboundPolicy;
use caudra_providers::{Message, PeerAssignment};
use caudra_storage::messages::{
    WorkFence, WorkFilter, WorkGroup, WorkItem, WorkOutcome, WorkState, Worker,
};
use caudra_storage::sessions::PermissionMode;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{
    Delivery, PeerHost, PeerSession, PolicyHold, Route, Sender, SessionInner, SessionState,
    WireMode, WorkCursor, WorkReported, lock, policy_hold, same_cohort, valid_handle, wall_ms,
};
use crate::{AgentMode, DoneReason};

pub const MAX_MEMBERSHIPS: usize = 8;
pub const MAX_OUTCOME_BYTES: usize = 4 * 1024;
/// How often an owner renews its lease, well inside the lease itself.
const HEARTBEAT: Duration = Duration::from_secs(20);
/// How often an idle member looks for work it may take.
const WORK_POLL: Duration = Duration::from_secs(3);
const MAX_OWNED: usize = 16;
const MAX_NOTICES: usize = 16;
/// Outcomes kept for a runtime that takes them; older ones give way.
const MAX_REPORTS: usize = 64;
pub const INVALID_GROUP: &str = "Consumer group names use 1 to 32 lowercase letters, digits, and hyphens, starting with a letter or digit";
pub const TOO_MANY_MEMBERSHIPS: &str = "A session joins at most 8 consumer groups";
pub const NOT_MEMBER: &str = "This session is not a member of that consumer group";
pub const INVALID_OUTCOME: &str = "A work outcome needs at most 4 KiB of text";
pub const COMPLETION_REQUIRED: &str =
    "Completion required: the turn ended without reporting an outcome";
pub const PAUSED_BY_CANCEL: &str = "The user cancelled the turn working on it";
pub(super) const TURN_LIMIT: &str = "The turn working on it reached its turn limit";
pub(super) const TURN_FAILED: &str = "The turn working on it failed";
pub(super) const SESSION_CLOSED: &str = "Its session closed while working on it";
const PAUSE_UNCONFIRMED: &str = "Could not record the pause of work";

/// How far this registration's assignment has come.
#[derive(Debug, PartialEq, Eq)]
enum Stage {
    /// Claimed from the history, waiting for a turn to take it in.
    Offered,
    /// In the claim of a turn taking it in.
    Claimed(u64),
    /// In the conversation.
    Started,
    /// Its turn was cancelled or its session closed, for the reason given;
    /// the item pauses once that turn stops.
    Pausing(&'static str),
}

pub(super) struct OwnedWork {
    item: WorkItem,
    token: String,
    stage: Stage,
    lease_until_ms: u64,
    sender: Sender,
    observation: Message,
}

/// The consumer-group work of one registration.
#[derive(Default)]
pub(super) struct Assignments {
    owned: Option<OwnedWork>,
    notices: VecDeque<WorkNotice>,
    reports: VecDeque<WorkReported>,
    acquiring: bool,
    polled: Option<Instant>,
}

/// What became of this session's work, for its person rather than its model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNotice {
    pub text: String,
    /// The turn working on the item should stop: another member may take the
    /// item over.
    pub stop: bool,
}

/// What a person may do to a work item, whichever member it was queued for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkAction {
    /// Queues a paused, failed, or cancelled item again with fresh attempts.
    Retry,
    /// Holds a queued item until a person retries it.
    Pause,
    /// Gives up on an item no member is working on.
    Cancel,
}

/// Work this session holds or paused, as its model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignedWork {
    pub work: String,
    pub group: String,
    pub state: String,
    pub attempt: u32,
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub publisher: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl From<&WorkItem> for AssignedWork {
    fn from(item: &WorkItem) -> Self {
        Self {
            work: item.name.clone(),
            group: item.group.clone(),
            state: item.state.as_str().into(),
            attempt: item.attempts,
            max_attempts: item.max_attempts,
            topic: item.message.message.audience.topic().map(str::to_owned),
            publisher: item.message.message.sender.name.clone(),
            reason: item.reason.clone(),
            result: item.result.clone(),
        }
    }
}

/// A work item as a person's retry, pause, or cancellation left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWork {
    pub item: AssignedWork,
    /// A retried item had claims before, so its next member may repeat
    /// effects those claims already had.
    pub repeats_claims: bool,
}

/// Pending work of one of this session's groups that its inbound policy
/// passes over for one reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedWork {
    pub group: String,
    pub hold: PolicyHold,
    pub count: u64,
}

/// What decides, outside the session lock, whether this registration may
/// take an item.
struct Admission {
    route: Route,
    inbound: InboundPolicy,
    mode: AgentMode,
    canonical_cwd: Option<PathBuf>,
    permission_mode: PermissionMode,
}

impl Admission {
    fn admits(&self, item: &WorkItem) -> bool {
        self.publisher(item)
            .is_some_and(|sender| self.hold(&sender).is_none())
    }

    /// Why this registration's inbound policy passes over `item`, if it does.
    fn passes_over(&self, item: &WorkItem) -> Option<PolicyHold> {
        self.publisher(item).and_then(|sender| self.hold(&sender))
    }

    /// The publisher of `item`, unless it is a ReadOnly session, whose work
    /// no member takes.
    fn publisher(&self, item: &WorkItem) -> Option<Sender> {
        Delivery::from_history(item.message.clone(), &self.route)
            .map(|delivery| delivery.sender)
            .filter(|sender| sender.mode != WireMode::ReadOnly)
    }

    fn hold(&self, sender: &Sender) -> Option<PolicyHold> {
        policy_hold(
            &self.inbound,
            sender,
            same_cohort(
                &self.mode,
                self.canonical_cwd.as_deref(),
                &self.permission_mode,
                sender,
            ),
        )
    }
}

impl Assignments {
    /// Whether an item waits for a turn to take it in.
    pub(super) fn offered(&self) -> bool {
        self.owned
            .as_ref()
            .is_some_and(|work| work.stage == Stage::Offered)
    }

    /// Puts offered work in claim `claim_id` and returns its framed assignment.
    pub(super) fn claim(&mut self, claim_id: u64) -> Option<Message> {
        let work = self
            .owned
            .as_mut()
            .filter(|work| work.stage == Stage::Offered)?;
        work.stage = Stage::Claimed(claim_id);
        Some(work.observation.clone())
    }

    /// Settles the work in claim `claim_id`: in the conversation once
    /// `taken`, else offered again.
    pub(super) fn settle_claim(&mut self, claim_id: u64, taken: bool) {
        if let Some(work) = self
            .owned
            .as_mut()
            .filter(|work| work.stage == Stage::Claimed(claim_id))
        {
            work.stage = if taken {
                Stage::Started
            } else {
                Stage::Offered
            };
        }
    }

    fn owns(&self, token: &str) -> bool {
        self.owned.as_ref().is_some_and(|work| work.token == token)
    }

    /// The name and token of the work a turn took in.
    fn taken(&self) -> Option<(String, String)> {
        self.owned
            .as_ref()
            .filter(|work| work.stage != Stage::Offered)
            .map(|work| (work.item.name.clone(), work.token.clone()))
    }

    fn notify(&mut self, text: String, stop: bool) {
        if self.notices.len() == MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(WorkNotice { text, stop });
    }

    fn record(&mut self, report: WorkReported) {
        if self.reports.len() == MAX_REPORTS {
            self.reports.pop_front();
        }
        self.reports.push_back(report);
    }
}

impl SessionState {
    /// Whether this registration may look for work now.
    fn can_take_work(&self) -> bool {
        self.open
            && !self.groups.is_empty()
            && self.work.owned.is_none()
            && self.reports_work()
            && self.approval_blocker().is_none()
    }

    /// Whether this registration's agent may report how work went, which a
    /// read-only one may not, so it would only stall the items it took.
    fn reports_work(&self) -> bool {
        !matches!(self.descriptor.mode, AgentMode::ReadOnly)
    }

    /// Whether this registration may take work from `sender`, as its
    /// inbound policy stands.
    fn admits_work(&self, sender: &Sender) -> bool {
        self.approval_blocker().is_none()
            && self.reports_work()
            && sender.mode != WireMode::ReadOnly
            && policy_hold(&self.descriptor.inbound, sender, self.same_cohort(sender)).is_none()
    }

    fn admission(&self, route: &Route) -> Admission {
        Admission {
            route: route.clone(),
            inbound: self.descriptor.inbound.clone(),
            mode: self.descriptor.mode.clone(),
            canonical_cwd: self.canonical_cwd.clone(),
            permission_mode: self.descriptor.permission_mode.clone(),
        }
    }

    /// Returns offered work this registration may no longer take to the
    /// queue, so another member may take it.
    pub(super) fn reevaluate_work(&mut self) {
        let release = self.work.owned.as_ref().is_some_and(|work| {
            work.stage == Stage::Offered
                && (!self.open
                    || !self.groups.contains(&work.item.group)
                    || !self.admits_work(&work.sender))
        });
        if release {
            self.release_offered();
        }
    }

    /// Returns offered work no turn took in to the queue without spending
    /// one of its attempts.
    fn release_offered(&mut self) {
        if let Some(work) = self.work.owned.take_if(|work| work.stage == Stage::Offered) {
            let OwnedWork { item, token, .. } = work;
            self.history
                .report(move |log| log.release_work(&item.name, &token, wall_ms()));
        }
    }

    /// Settles the work of a registration that is closing: offered work
    /// returns to the queue, and work a turn took in pauses, keeping its
    /// lease while that turn may still run.
    pub(super) fn close_work(&mut self) {
        self.release_offered();
        let busy = self.descriptor.busy;
        let Some(work) = self
            .work
            .owned
            .as_mut()
            .filter(|work| matches!(work.stage, Stage::Claimed(_) | Stage::Started))
        else {
            return;
        };
        let (name, token) = (work.item.name.clone(), work.token.clone());
        if busy {
            work.stage = Stage::Pausing(SESSION_CLOSED);
        } else {
            let group = work.item.group.clone();
            self.work.owned = None;
            self.work
                .record(WorkReported::paused(group, name.clone(), SESSION_CLOSED));
        }
        self.history.report(move |log| {
            log.pause_work(&name, &token, !busy, SESSION_CLOSED, wall_ms())
                .map(drop)
        });
    }
}

/// Validates a consumer group name, which follows the messaging-name grammar.
pub fn parse_group(value: &str) -> Result<String, String> {
    if valid_handle(value) {
        Ok(value.to_owned())
    } else {
        Err(INVALID_GROUP.into())
    }
}

pub(super) fn check_memberships(groups: &[String]) -> Result<(), String> {
    if groups.len() > MAX_MEMBERSHIPS {
        return Err(TOO_MANY_MEMBERSHIPS.into());
    }
    let unique: HashSet<_> = groups.iter().collect();
    if unique.len() != groups.len() || !groups.iter().all(|group| valid_handle(group)) {
        return Err(INVALID_GROUP.into());
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn valid_memberships(groups: &[String]) -> bool {
    check_memberships(groups).is_ok()
}

fn check_outcome(outcome: &WorkOutcome) -> Result<(), String> {
    let text = match outcome {
        WorkOutcome::Completed(summary) => summary.as_deref().unwrap_or_default(),
        WorkOutcome::Retry(reason) | WorkOutcome::Failed(reason) => reason,
    };
    if text.len() > MAX_OUTCOME_BYTES {
        return Err(INVALID_OUTCOME.into());
    }
    Ok(())
}

/// Why a turn that took work in ended without reporting an outcome.
/// `ending` is `None` when the turn failed.
fn pause_reason(stage: &Stage, ending: Option<DoneReason>) -> &'static str {
    match (stage, ending) {
        (Stage::Pausing(reason), _) => reason,
        (_, Some(DoneReason::Cancelled)) => PAUSED_BY_CANCEL,
        (_, Some(DoneReason::EndTurn | DoneReason::MaxTokens)) => COMPLETION_REQUIRED,
        (_, Some(DoneReason::MaxTurns)) => TURN_LIMIT,
        (_, None) => TURN_FAILED,
    }
}

/// Renews the lease of the work `token` claimed while `session` owns it.
fn heartbeat(session: Weak<SessionInner>, token: String) {
    smol::spawn(async move {
        loop {
            smol::Timer::after(HEARTBEAT).await;
            let Some(session) = session.upgrade() else {
                return;
            };
            if !PeerSession(session).renew_work(&token).await {
                return;
            }
        }
    })
    .detach();
}

impl PeerSession {
    /// The consumer groups this session takes work from.
    pub fn groups(&self) -> Vec<String> {
        lock(&self.0.state).groups.clone()
    }

    /// Joins the existing consumer group `name`, under the policy its creator
    /// set. A missing group is an error rather than an empty group.
    pub async fn join_group(&self, name: &str) -> Result<(), String> {
        let name = parse_group(name)?;
        {
            let state = lock(&self.0.state);
            state.ensure_open()?;
            if state.groups.contains(&name) {
                return Ok(());
            }
            if state.groups.len() >= MAX_MEMBERSHIPS {
                return Err(TOO_MANY_MEMBERSHIPS.into());
            }
        }
        let group = name.clone();
        self.0
            .host
            .history
            .query(move |log| log.group(&group).map(drop))
            .await?;
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        if !state.groups.contains(&name) {
            if state.groups.len() >= MAX_MEMBERSHIPS {
                return Err(TOO_MANY_MEMBERSHIPS.into());
            }
            state.groups.push(name);
            state.work.polled = None;
        }
        self.0.host.changed.notify(usize::MAX);
        Ok(())
    }

    /// Stops taking new work from `name`. Work a turn already took in stays
    /// this session's until it reports an outcome.
    pub fn leave_group(&self, name: &str) -> Result<(), String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        if !state.groups.iter().any(|group| group == name) {
            return Err(NOT_MEMBER.into());
        }
        state.groups.retain(|group| group != name);
        state.reevaluate_work();
        self.0.host.changed.notify(usize::MAX);
        Ok(())
    }

    /// Looks for work in the background, at most every `WORK_POLL`, while
    /// this session has room for an assignment. Claimed work then waits for
    /// a turn as a message does.
    pub fn poll_work(&self) {
        let now = Instant::now();
        {
            let mut state = lock(&self.0.state);
            if state.work.acquiring
                || !state.can_take_work()
                || state
                    .work
                    .polled
                    .is_some_and(|at| now.saturating_duration_since(at) < WORK_POLL)
            {
                return;
            }
            state.work.acquiring = true;
            state.work.polled = Some(now);
        }
        let session = self.clone();
        smol::spawn(async move {
            if let Err(error) = session.acquire_work().await {
                warn!(%error, "claiming consumer group work failed");
            }
            lock(&session.0.state).work.acquiring = false;
        })
        .detach();
    }

    /// Returns work no turn took in to the queue, for a session that cannot
    /// start a turn for it now, so it never holds an item it is not working on.
    pub fn release_offered_work(&self) {
        lock(&self.0.state).release_offered();
    }

    /// Claims the oldest item of this session's groups that it may take, and
    /// offers it to the next turn. False when there was none to take or the
    /// session cannot take work now.
    pub async fn acquire_work(&self) -> Result<bool, String> {
        let (worker, groups, admission) = {
            let state = lock(&self.0.state);
            if !state.can_take_work() {
                return Ok(false);
            }
            let worker = Worker {
                session: self.session_id().to_string(),
                route: self.0.route.target(),
                name: Some(state.descriptor.name.clone()),
                handle: state.claimed_handle(),
            };
            (worker, state.groups.clone(), state.admission(&self.0.route))
        };
        let claimed = self
            .0
            .host
            .history
            .query(move |log| {
                log.claim_work(&worker, &groups, wall_ms(), |item| admission.admits(item))
            })
            .await?;
        let Some(assignment) = claimed else {
            return Ok(false);
        };
        let (item, token) = (assignment.work, assignment.token);
        let mut state = lock(&self.0.state);
        let delivery =
            Delivery::from_history(item.message.clone(), &self.0.route).filter(|delivery| {
                state.can_take_work()
                    && state.groups.contains(&item.group)
                    && state.admits_work(&delivery.sender)
            });
        let naming = delivery.map(|delivery| (state.naming(&delivery), delivery));
        let Some((Ok(naming), delivery)) = naming else {
            let (name, release) = (item.name.clone(), token.clone());
            state
                .history
                .report(move |log| log.release_work(&name, &release, wall_ms()));
            return Ok(false);
        };
        let origin = naming.origin(
            &delivery,
            None,
            Some(PeerAssignment {
                group: item.group.clone(),
                work: item.name.clone(),
                attempt: item.attempts,
                max_attempts: item.max_attempts,
            }),
        );
        state.bind_names(naming);
        state.work.owned = Some(OwnedWork {
            lease_until_ms: item.lease_until_ms.unwrap_or_default(),
            observation: Message::peer_observation(delivery.text, origin),
            sender: delivery.sender,
            item,
            token: token.clone(),
            stage: Stage::Offered,
        });
        drop(state);
        heartbeat(Arc::downgrade(&self.0), token);
        self.0.host.changed.notify(usize::MAX);
        Ok(true)
    }

    /// Extends the lease of the work `token` claimed. False once this
    /// registration closed or no longer owns that work, which ends its
    /// heartbeat: closing paused the work, so its lease may lapse.
    async fn renew_work(&self, token: &str) -> bool {
        let name = {
            let state = lock(&self.0.state);
            match &state.work.owned {
                Some(work) if state.open && work.token == token => work.item.name.clone(),
                _ => return false,
            }
        };
        let renewal = {
            let (name, token) = (name.clone(), token.to_owned());
            self.0
                .host
                .history
                .decide(move |log| log.renew_work(&name, &token, wall_ms()))
                .await
        };
        let mut state = lock(&self.0.state);
        let Some(work) = state.work.owned.as_mut().filter(|work| work.token == token) else {
            return false;
        };
        let lost = match renewal {
            Ok(Ok(until)) => {
                work.lease_until_ms = until;
                return true;
            }
            Ok(Err(refusal)) => refusal.to_string(),
            Err(error) if wall_ms() < work.lease_until_ms => {
                warn!(%error, work = %name, "renewing a work lease failed");
                return true;
            }
            Err(error) => error,
        };
        let stop = work.stage != Stage::Offered;
        let group = work.item.group.clone();
        state.work.owned = None;
        if stop {
            state.work.notify(
                format!(
                    "Lost work {name} of group {group}: {lost}. Stopping the turn working on it."
                ),
                true,
            );
        }
        self.0.host.changed.notify(usize::MAX);
        false
    }

    /// Reports how the work named `work` went: the item a turn of this
    /// session took in, or one it owned before it paused. Repeating a
    /// completion is harmless.
    pub async fn report_work(
        &self,
        work: &str,
        outcome: WorkOutcome,
    ) -> Result<AssignedWork, String> {
        check_outcome(&outcome)?;
        let (fence, token) = {
            let state = lock(&self.0.state);
            state.ensure_open()?;
            match state.work.taken().filter(|(name, _)| name == work) {
                Some((_, token)) => (WorkFence::Lease(token.clone()), Some(token)),
                None => (WorkFence::Owner(self.session_id().to_string()), None),
            }
        };
        let name = work.to_owned();
        let reported = self
            .0
            .host
            .history
            .decide(move |log| log.finish_work(&name, &fence, &outcome, wall_ms()))
            .await?;
        let mut state = lock(&self.0.state);
        if let Some(token) = token
            && state.work.owns(&token)
        {
            state.work.owned = None;
            self.0.host.changed.notify(usize::MAX);
        }
        let item = reported.map_err(|refusal| refusal.to_string())?;
        if let Some(report) = WorkReported::reported(&item) {
            state.work.record(report);
        }
        Ok(AssignedWork::from(&item))
    }

    /// The work a turn of this session took in, then the paused work it last
    /// owned, newest first.
    pub async fn owned_work(&self) -> Result<Vec<AssignedWork>, String> {
        let current = {
            let state = lock(&self.0.state);
            state.ensure_open()?;
            state.work.taken().map(|(name, _)| name)
        };
        let filter = WorkFilter {
            owner: Some(self.session_id().to_string()),
            states: vec![WorkState::Leased, WorkState::Pausing, WorkState::Paused],
            ..WorkFilter::default()
        };
        let items = self
            .0
            .host
            .history
            .query(move |log| log.work(&filter, None, MAX_OWNED))
            .await?;
        let mut owned: Vec<_> = items
            .iter()
            .filter(|item| item.state == WorkState::Paused || current.as_ref() == Some(&item.name))
            .map(AssignedWork::from)
            .collect();
        owned.sort_by_key(|work| current.as_ref() != Some(&work.work));
        Ok(owned)
    }

    /// Records, before a cancellation reaches the turn working on this
    /// session's work, that the item pauses rather than returning to the
    /// queue. Offered work no turn took in returns to the queue instead.
    pub async fn request_pause(&self) -> Result<(), String> {
        let (name, token) = {
            let mut state = lock(&self.0.state);
            state.release_offered();
            let Some(work) = state
                .work
                .owned
                .as_mut()
                .filter(|work| matches!(work.stage, Stage::Claimed(_) | Stage::Started))
            else {
                return Ok(());
            };
            work.stage = Stage::Pausing(PAUSED_BY_CANCEL);
            (work.item.name.clone(), work.token.clone())
        };
        let pause = {
            let (name, token) = (name.clone(), token.clone());
            self.0
                .host
                .history
                .decide(move |log| {
                    log.pause_work(&name, &token, false, PAUSED_BY_CANCEL, wall_ms())
                })
                .await
        };
        match pause {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(refusal)) => {
                let mut state = lock(&self.0.state);
                if state.work.owns(&token) {
                    state.work.owned = None;
                }
                Err(refusal.to_string())
            }
            Err(error) => Err(format!("{PAUSE_UNCONFIRMED} {name}: {error}")),
        }
    }

    /// Pauses the work the ended turn took in without reporting an outcome,
    /// so it waits for a person instead of another member. `ending` is
    /// `None` when the turn failed.
    pub async fn settle_work(&self, ending: Option<DoneReason>) {
        let taken = lock(&self.0.state)
            .work
            .owned
            .as_ref()
            .filter(|work| work.stage != Stage::Offered)
            .map(|work| {
                (
                    work.item.group.clone(),
                    work.item.name.clone(),
                    work.token.clone(),
                    pause_reason(&work.stage, ending),
                )
            });
        let Some((group, name, token, reason)) = taken else {
            return;
        };
        let pause = {
            let (name, token) = (name.clone(), token.clone());
            self.0
                .host
                .history
                .decide(move |log| log.pause_work(&name, &token, true, reason, wall_ms()))
                .await
        };
        let mut state = lock(&self.0.state);
        if state.work.owns(&token) {
            state.work.owned = None;
        }
        let text = match pause {
            Ok(Ok(_)) => {
                state
                    .work
                    .record(WorkReported::paused(group, name.clone(), reason));
                format!("Paused work {name}: {reason}. /groups to retry or cancel it")
            }
            Ok(Err(refusal)) => refusal.to_string(),
            Err(error) => format!(
                "{PAUSE_UNCONFIRMED} {name}: {error}; it returns to the queue once its lease lapses"
            ),
        };
        state.work.notify(text, false);
        self.0.host.changed.notify(usize::MAX);
    }

    /// Counts the pending work of this session's groups that its inbound
    /// policy passes over, by group and reason. Its own publications, which
    /// it never takes, do not count.
    pub async fn skipped_work(&self) -> Result<Vec<SkippedWork>, String> {
        let (groups, admission) = {
            let state = lock(&self.0.state);
            state.ensure_open()?;
            (state.groups.clone(), state.admission(&self.0.route))
        };
        let session = self.session_id().to_string();
        let counts = self
            .0
            .host
            .history
            .query(move |log| {
                let mut counts: BTreeMap<(String, PolicyHold), u64> = BTreeMap::new();
                log.for_each_pending(&groups, &session, |item| {
                    if let Some(hold) = admission.passes_over(&item) {
                        *counts.entry((item.group, hold)).or_insert(0) += 1;
                    }
                })?;
                Ok(counts)
            })
            .await?;
        Ok(counts
            .into_iter()
            .map(|((group, hold), count)| SkippedWork { group, hold, count })
            .collect())
    }

    pub fn take_work_notices(&self) -> Vec<WorkNotice> {
        lock(&self.0.state).work.notices.drain(..).collect()
    }

    /// What became of this session's group work since the last call, oldest
    /// first.
    pub fn take_work_reports(&self) -> Vec<WorkReported> {
        lock(&self.0.state).work.reports.drain(..).collect()
    }

    /// The stamp of the latest change to any work item, where a cursor over
    /// this session's published work starts.
    pub async fn work_cursor(&self) -> Result<WorkCursor, String> {
        self.0
            .host
            .history
            .query(|log| log.last_work_change())
            .await
            .map(WorkCursor)
    }

    /// Up to `limit` items that this session's publications queued and that
    /// changed after `after`, with their stamps, oldest change first. Two
    /// changes between calls show as the item's latest state.
    pub async fn published_work_since(
        &self,
        after: WorkCursor,
        limit: usize,
    ) -> Result<Vec<(WorkCursor, WorkItem)>, String> {
        let filter = WorkFilter {
            publisher: Some(self.session_id().to_string()),
            ..WorkFilter::default()
        };
        let items = self
            .0
            .host
            .history
            .query(move |log| log.work_changed_after(&filter, after.0, limit))
            .await?;
        Ok(items
            .into_iter()
            .map(|item| (WorkCursor(item.changed), item))
            .collect())
    }

    /// Every consumer group, for a person choosing what to join.
    pub async fn consumer_groups(&self) -> Result<Vec<WorkGroup>, String> {
        self.0.host.history.query(|log| log.groups()).await
    }

    /// Retries, pauses, or cancels the item `work` for this session's person,
    /// whichever member it was queued for.
    pub async fn manage_work(&self, work: &str, action: WorkAction) -> Result<ManagedWork, String> {
        let name = work.to_owned();
        let held = action == WorkAction::Pause;
        let (item, repeats_claims) = self
            .0
            .host
            .history
            .query(move |log| match action {
                WorkAction::Retry => {
                    let claimed = !log.work_detail(&name)?.attempts.is_empty();
                    Ok((log.retry_work(&name, wall_ms())?, claimed))
                }
                WorkAction::Pause => Ok((log.hold_work(&name, wall_ms())?, false)),
                WorkAction::Cancel => Ok((log.cancel_work(&name, wall_ms())?, false)),
            })
            .await?;
        let mut state = lock(&self.0.state);
        state.work.polled = None;
        if held && let Some(reason) = &item.reason {
            state.work.record(WorkReported::paused(
                item.group.clone(),
                item.name.clone(),
                reason,
            ));
        }
        Ok(ManagedWork {
            item: AssignedWork::from(&item),
            repeats_claims,
        })
    }
}

impl PeerHost {
    /// Fails unless every group in `groups` exists, so a session never waits
    /// on a group nobody created.
    pub async fn check_groups(&self, groups: &[String]) -> Result<(), String> {
        let groups = groups.to_vec();
        self.0
            .history
            .query(move |log| {
                groups
                    .iter()
                    .try_for_each(|group| log.group(group).map(drop))
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{INVALID_GROUP, MAX_MEMBERSHIPS, TOO_MANY_MEMBERSHIPS, check_memberships};
    use test_case::test_case;

    const GROUP: &str = "ci-triage";
    const OTHER_GROUP: &str = "deploy-watch";
    const MALFORMED_GROUP: &str = "CI_Triage";

    #[test_case(&[GROUP, OTHER_GROUP], None; "distinct_groups")]
    #[test_case(&[GROUP, GROUP], Some(INVALID_GROUP); "duplicate")]
    #[test_case(&[MALFORMED_GROUP], Some(INVALID_GROUP); "malformed")]
    fn memberships_are_validated(groups: &[&str], error: Option<&str>) {
        let groups: Vec<String> = groups.iter().copied().map(str::to_owned).collect();
        assert_eq!(check_memberships(&groups).err().as_deref(), error);
    }

    #[test]
    fn memberships_are_bounded() {
        let groups: Vec<String> = (0..=MAX_MEMBERSHIPS)
            .map(|index| format!("{GROUP}-{index}"))
            .collect();
        assert!(check_memberships(&groups[..MAX_MEMBERSHIPS]).is_ok());
        assert_eq!(
            check_memberships(&groups).unwrap_err(),
            TOO_MANY_MEMBERSHIPS
        );
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use std::path::Path;

    use caudra_automation::event::WorkOutcome as ReportedOutcome;
    use caudra_config::{InboundPolicy, MessagingConfig};
    use caudra_providers::{PeerAssignment, PeerAudience};
    use caudra_storage::messages::{
        DEFAULT_MAX_ATTEMPTS, GroupPolicy, LEASE_MS, MAX_GROUPS, MAX_OUTSTANDING, MessageAudience,
        NewMessage, WorkItem, WorkOutcome, WorkRefusal, WorkState, Worker,
    };
    use caudra_storage::sessions::StoredPeerControls;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        COMPLETION_REQUIRED, MAX_MEMBERSHIPS, NOT_MEMBER, PAUSED_BY_CANCEL, SESSION_CLOSED,
        SkippedWork, TOO_MANY_MEMBERSHIPS, TURN_FAILED, TURN_LIMIT, WorkAction, check_memberships,
    };
    use crate::peers::script::tests::script;
    use crate::peers::tests::{Recorder, descriptor, directory, host, observe};
    use crate::peers::{
        PeerDescriptor, PeerHost, PeerSession, PolicyHold, PublishReceipt, SendFailure,
        SendFailureKind, SendOrigin, WorkCursor, WorkPause, WorkReported, lock, recipient_room,
        token, wall_ms,
    };
    use crate::{AgentMode, DoneReason};

    const GROUP: &str = "ci-triage";
    const OTHER_GROUP: &str = "deploy-watch";
    const MISSING_GROUP: &str = "no-such-group";
    const PATTERN: &str = "ci.*";
    const TOPIC: &str = "ci.failures";
    const TEXT: &str = "The nightly build failed";
    const REQUEST_ID: &str = "nightly-failure";
    const SUMMARY: &str = "Fixed the flaky linker step";
    const FAILURE: &str = "The runner ran out of disk";
    const LEASED: &str = "leased";
    const COMPLETED: &str = "completed";
    const LOST: &str = "Lost work";
    const RECOVERY: &str = "lease-recovery";
    const SCRIPT_LABEL: &str = "nightly-ci";
    const AUTOMATION: &str = "nightly-digest";
    const PAGE: usize = 8;
    const SINGLE: usize = 1;
    const SINGLE_BACKLOG: u32 = 1;
    const BOTH_GROUPS: [&str; 2] = [GROUP, OTHER_GROUP];

    async fn grouped() -> (TempDir, PeerHost, PeerSession) {
        let directory = directory();
        let host = host(directory.path());
        let publisher = host
            .register(descriptor(directory.path(), InboundPolicy::Auto))
            .unwrap();
        create_group(&publisher, GROUP, GroupPolicy::default()).await;
        (directory, host, publisher)
    }

    async fn create_group(session: &PeerSession, name: &str, policy: GroupPolicy) {
        let name = name.to_owned();
        session
            .0
            .host
            .history
            .query(move |log| {
                log.create_group(&name, &[PATTERN.to_owned()], &policy, wall_ms())
                    .map(drop)
            })
            .await
            .unwrap();
    }

    fn member(host: &PeerHost, cwd: &Path, inbound: InboundPolicy) -> PeerSession {
        host.register_with_controls(
            descriptor(cwd, inbound),
            &MessagingConfig::default(),
            Some(StoredPeerControls {
                groups: vec![GROUP.into()],
                ..StoredPeerControls::default()
            }),
        )
        .unwrap()
    }

    async fn publish(publisher: &PeerSession) -> String {
        let audience = PeerAudience::Topic {
            topic: TOPIC.into(),
        };
        let receipt = publisher.publish(audience, TEXT, REQUEST_ID).await.unwrap();
        receipt.queued[0].work.clone()
    }

    async fn item(session: &PeerSession, work: &str) -> WorkItem {
        let work = work.to_owned();
        session
            .0
            .host
            .history
            .query(move |log| log.work_item(&work))
            .await
            .unwrap()
    }

    /// Claims the oldest item and takes it into a turn, as a wake does.
    async fn start(worker: &PeerSession) {
        assert!(worker.acquire_work().await.unwrap());
        worker.claim_wake().unwrap().commit();
    }

    #[test]
    fn claimed_work_wakes_one_turn_and_stays_owned_until_reported() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            assert!(!worker.has_pending());
            assert!(worker.acquire_work().await.unwrap());
            assert!(worker.has_pending());
            assert!(worker.claim().is_none());
            drop(worker.claim_wake().unwrap());
            let claim = worker.claim_wake().unwrap();
            let origin = claim.messages()[0].peer_event.clone().unwrap();
            assert_eq!(
                origin.assignment,
                Some(PeerAssignment {
                    group: GROUP.into(),
                    work: work.clone(),
                    attempt: 1,
                    max_attempts: DEFAULT_MAX_ATTEMPTS,
                })
            );
            claim.commit();
            assert!(!worker.has_pending());
            assert!(!worker.acquire_work().await.unwrap());
            let owned = worker.owned_work().await.unwrap();
            assert_eq!(owned.len(), 1);
            assert_eq!(
                (owned[0].work.as_str(), owned[0].state.as_str()),
                (work.as_str(), LEASED)
            );
            let outcome = WorkOutcome::Completed(Some(SUMMARY.into()));
            let reported = worker.report_work(&work, outcome.clone()).await.unwrap();
            assert_eq!(
                (reported.state.as_str(), reported.result.as_deref()),
                (COMPLETED, Some(SUMMARY))
            );
            assert_eq!(worker.report_work(&work, outcome).await.unwrap(), reported);
            assert!(worker.owned_work().await.unwrap().is_empty());
        });
    }

    #[test_case(Some(DoneReason::EndTurn), COMPLETION_REQUIRED; "end_turn")]
    #[test_case(Some(DoneReason::MaxTokens), COMPLETION_REQUIRED; "max_tokens")]
    #[test_case(Some(DoneReason::MaxTurns), TURN_LIMIT; "turn_limit")]
    #[test_case(Some(DoneReason::Cancelled), PAUSED_BY_CANCEL; "cancelled")]
    #[test_case(None, TURN_FAILED; "failed")]
    fn turns_ending_without_an_outcome_pause_their_work(ending: Option<DoneReason>, reason: &str) {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            start(&worker).await;
            worker.settle_work(ending).await;
            let paused = item(&publisher, &work).await;
            assert_eq!(
                (paused.state, paused.reason.as_deref()),
                (WorkState::Paused, Some(reason))
            );
            let notices = worker.take_work_notices();
            assert!(
                notices.len() == 1 && notices[0].text.contains(reason) && !notices[0].stop,
                "{notices:?}"
            );
            let other = member(&host, directory.path(), InboundPolicy::Auto);
            assert!(!other.acquire_work().await.unwrap());
            let reported = worker
                .report_work(&work, WorkOutcome::Completed(None))
                .await
                .unwrap();
            assert_eq!(reported.state, COMPLETED);
        });
    }

    #[test]
    fn cancelling_pauses_taken_work_and_returns_offered_work_to_the_queue() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            assert!(worker.acquire_work().await.unwrap());
            worker.request_pause().await.unwrap();
            let queued = item(&publisher, &work).await;
            assert_eq!((queued.state, queued.attempts), (WorkState::Pending, 0));
            start(&worker).await;
            worker.request_pause().await.unwrap();
            assert_eq!(item(&publisher, &work).await.state, WorkState::Pausing);
            let other = member(&host, directory.path(), InboundPolicy::Auto);
            assert!(!other.acquire_work().await.unwrap());
            worker.settle_work(Some(DoneReason::EndTurn)).await;
            let paused = item(&publisher, &work).await;
            assert_eq!(
                (paused.state, paused.reason.as_deref()),
                (WorkState::Paused, Some(PAUSED_BY_CANCEL))
            );
            assert!(!other.acquire_work().await.unwrap());
        });
    }

    #[test_case(false, WorkState::Paused; "idle")]
    #[test_case(true, WorkState::Pausing; "busy")]
    fn closing_pauses_taken_work_for_the_closed_session(busy: bool, closed: WorkState) {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            start(&worker).await;
            worker
                .update(PeerDescriptor {
                    busy,
                    ..worker.descriptor()
                })
                .unwrap();
            worker.close();
            let item_after_close = item(&publisher, &work).await;
            assert_eq!(
                (item_after_close.state, item_after_close.reason.as_deref()),
                (closed, Some(SESSION_CLOSED))
            );
            worker.settle_work(Some(DoneReason::Cancelled)).await;
            let paused = item(&publisher, &work).await;
            assert_eq!(
                (paused.state, paused.reason.as_deref()),
                (WorkState::Paused, Some(SESSION_CLOSED))
            );
        });
    }

    #[test_case(|worker: &PeerSession| worker.leave_group(GROUP).unwrap(); "left_the_group")]
    #[test_case(|worker: &PeerSession| worker.set_inbound(InboundPolicy::Hold).unwrap(); "held_by_policy")]
    #[test_case(PeerSession::suppress_wakes; "wakes_suppressed")]
    #[test_case(PeerSession::close; "closed")]
    fn offered_work_returns_to_the_queue_once_its_member_may_not_take_it(change: fn(&PeerSession)) {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            assert!(worker.acquire_work().await.unwrap());
            change(&worker);
            assert!(!worker.has_pending());
            let other = member(&host, directory.path(), InboundPolicy::Auto);
            assert!(other.acquire_work().await.unwrap());
            assert_eq!(item(&publisher, &work).await.attempts, 1);
        });
    }

    #[test_case(InboundPolicy::Accept, true; "accept")]
    #[test_case(InboundPolicy::Auto, true; "auto_in_cohort")]
    #[test_case(InboundPolicy::Hold, false; "hold")]
    #[test_case(InboundPolicy::Refuse, false; "refuse")]
    fn members_take_only_work_their_inbound_policy_admits(inbound: InboundPolicy, takes: bool) {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), inbound);
            publish(&publisher).await;
            assert_eq!(worker.acquire_work().await.unwrap(), takes);
        });
    }

    #[test_case(InboundPolicy::Accept, &[]; "accept_skips_nothing")]
    #[test_case(InboundPolicy::Auto, &[(PolicyHold::Script, 1), (PolicyHold::Cohort, 1)]; "auto_skips_scripts_and_other_cohorts")]
    #[test_case(InboundPolicy::Hold, &[(PolicyHold::Policy, 3)]; "hold_skips_all")]
    #[test_case(InboundPolicy::Refuse, &[(PolicyHold::Policy, 3)]; "refuse_skips_all")]
    fn skipped_work_counts_what_the_inbound_policy_passes_over(
        inbound: InboundPolicy,
        expected: &[(PolicyHold, u64)],
    ) {
        smol::block_on(async {
            let elsewhere = directory();
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), inbound);
            let outsider = host
                .register(descriptor(elsewhere.path(), InboundPolicy::Auto))
                .unwrap();
            for session in [&publisher, &outsider, &worker] {
                publish(session).await;
            }
            let audience = PeerAudience::Topic {
                topic: TOPIC.into(),
            };
            script(&directory, &MessagingConfig::default(), SCRIPT_LABEL)
                .publish(audience, TEXT)
                .await
                .unwrap();
            let skipped = worker.skipped_work().await.unwrap();
            let expected: Vec<SkippedWork> = expected
                .iter()
                .map(|(hold, count)| SkippedWork {
                    group: GROUP.into(),
                    hold: hold.clone(),
                    count: *count,
                })
                .collect();
            assert_eq!(skipped, expected);
        });
    }

    #[test]
    fn people_manage_work_of_groups_that_exist() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            host.check_groups(&[GROUP.into()]).await.unwrap();
            let missing = host
                .check_groups(&[GROUP.into(), MISSING_GROUP.into()])
                .await
                .unwrap_err();
            assert!(missing.contains(MISSING_GROUP), "{missing}");
            let groups = worker.consumer_groups().await.unwrap();
            assert_eq!(
                groups
                    .iter()
                    .map(|group| group.name.as_str())
                    .collect::<Vec<_>>(),
                [GROUP]
            );
            let work = publish(&publisher).await;
            let paused = worker.manage_work(&work, WorkAction::Pause).await.unwrap();
            assert_eq!(paused.item.state, WorkState::Paused.as_str());
            assert!(!worker.acquire_work().await.unwrap());
            let retried = worker.manage_work(&work, WorkAction::Retry).await.unwrap();
            assert_eq!(retried.item.state, WorkState::Pending.as_str());
            let cancelled = worker.manage_work(&work, WorkAction::Cancel).await.unwrap();
            assert_eq!(cancelled.item.state, WorkState::Cancelled.as_str());
            assert!(worker.manage_work(&work, WorkAction::Pause).await.is_err());
            assert!(!worker.acquire_work().await.unwrap());
            assert_eq!(worker.leave_group(OTHER_GROUP).unwrap_err(), NOT_MEMBER);
        });
    }

    #[test_case(false; "held_before_any_claim")]
    #[test_case(true; "failed_after_a_claim")]
    fn retries_warn_only_of_claims_whose_effects_may_repeat(claimed: bool) {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            if claimed {
                start(&worker).await;
                worker
                    .report_work(&work, WorkOutcome::Failed(FAILURE.into()))
                    .await
                    .unwrap();
            } else {
                worker.manage_work(&work, WorkAction::Pause).await.unwrap();
            }
            let retried = worker.manage_work(&work, WorkAction::Retry).await.unwrap();
            assert_eq!(
                (retried.item.state.as_str(), retried.repeats_claims),
                (WorkState::Pending.as_str(), claimed)
            );
        });
    }

    #[test]
    fn read_only_members_take_no_work_until_they_can_report() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Accept);
            let build = worker.descriptor();
            worker
                .update(PeerDescriptor {
                    mode: AgentMode::ReadOnly,
                    ..build.clone()
                })
                .unwrap();
            publish(&publisher).await;
            assert!(!worker.acquire_work().await.unwrap());
            worker.update(build).unwrap();
            assert!(worker.acquire_work().await.unwrap());
        });
    }

    #[test]
    fn publishers_never_take_their_own_work() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            publisher.join_group(GROUP).await.unwrap();
            publish(&publisher).await;
            assert!(!publisher.acquire_work().await.unwrap());
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            assert!(worker.acquire_work().await.unwrap());
        });
    }

    #[test]
    fn lost_leases_stop_the_turn_and_refuse_stale_reports() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            start(&worker).await;
            let token = lock(&worker.0.state)
                .work
                .owned
                .as_ref()
                .unwrap()
                .token
                .clone();
            assert!(worker.renew_work(&token).await);
            let lapsed = wall_ms() + 2 * LEASE_MS;
            let recovery = Worker {
                session: RECOVERY.into(),
                route: RECOVERY.into(),
                name: None,
                handle: None,
            };
            publisher
                .0
                .host
                .history
                .query(move |log| log.claim_work(&recovery, &[], lapsed, |_| false))
                .await
                .unwrap();
            assert_eq!(item(&publisher, &work).await.state, WorkState::Pending);
            assert!(!worker.renew_work(&token).await);
            let notices = worker.take_work_notices();
            assert!(
                notices.len() == 1 && notices[0].stop && notices[0].text.starts_with(LOST),
                "{notices:?}"
            );
            assert!(
                worker
                    .report_work(&work, WorkOutcome::Completed(None))
                    .await
                    .is_err()
            );
            assert!(worker.owned_work().await.unwrap().is_empty());
        });
    }

    #[test]
    fn sessions_join_a_bounded_number_of_existing_groups() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = host
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let missing = worker.join_group(MISSING_GROUP).await.unwrap_err();
            assert!(missing.contains(MISSING_GROUP), "{missing}");
            let names: Vec<String> = (0..=MAX_MEMBERSHIPS)
                .map(|index| format!("{OTHER_GROUP}-{index}"))
                .collect();
            for name in &names {
                create_group(&publisher, name, GroupPolicy::default()).await;
            }
            for name in &names[..MAX_MEMBERSHIPS] {
                worker.join_group(name).await.unwrap();
            }
            assert_eq!(
                worker
                    .join_group(&names[MAX_MEMBERSHIPS])
                    .await
                    .unwrap_err(),
                TOO_MANY_MEMBERSHIPS
            );
            assert_eq!(worker.controls().groups, names[..MAX_MEMBERSHIPS]);
            assert_eq!(check_memberships(&names).unwrap_err(), TOO_MANY_MEMBERSHIPS);
            worker.leave_group(&names[0]).unwrap();
            worker.join_group(GROUP).await.unwrap();
            assert!(worker.groups().iter().any(|group| group == GROUP));
        });
    }

    fn topic() -> PeerAudience {
        PeerAudience::Topic {
            topic: TOPIC.into(),
        }
    }

    /// Publishes the `index`th of distinct messages as `origin` sends them.
    async fn publish_as(
        publisher: &PeerSession,
        origin: &SendOrigin,
        index: usize,
    ) -> Result<PublishReceipt, SendFailure> {
        let text = format!("{TEXT} {index}");
        let request_id = format!("{REQUEST_ID}-{index}");
        publisher
            .publish_from(origin, topic(), &text, &request_id)
            .await
    }

    fn names(page: &[(WorkCursor, WorkItem)]) -> Vec<&str> {
        page.iter().map(|(_, item)| item.name.as_str()).collect()
    }

    #[test_case(InboundPolicy::Auto, false, true; "auto_in_cohort")]
    #[test_case(InboundPolicy::Auto, true, false; "auto_elsewhere")]
    #[test_case(InboundPolicy::Accept, true, true; "accept_elsewhere")]
    fn members_judge_marked_publications_as_their_session(
        inbound: InboundPolicy,
        elsewhere: bool,
        takes: bool,
    ) {
        smol::block_on(async {
            let other = directory();
            let (directory, host, publisher) = grouped().await;
            let cwd = if elsewhere {
                other.path()
            } else {
                directory.path()
            };
            let worker = member(&host, cwd, inbound);
            let automation = SendOrigin::Automation(AUTOMATION.into());
            publish_as(&publisher, &automation, 0).await.unwrap();
            assert_eq!(worker.acquire_work().await.unwrap(), takes);
            if takes {
                let claim = worker.claim_wake().unwrap();
                let origin = claim.messages()[0].peer_event.clone().unwrap();
                assert_eq!(origin.automation.as_deref(), Some(AUTOMATION));
            }
        });
    }

    #[test]
    fn group_work_never_reaches_the_observer() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let recorder = observe(&worker, Recorder::taking(AUTOMATION));
            publish(&publisher).await;
            assert!(worker.acquire_work().await.unwrap());
            assert!(worker.claim_wake().is_some());
            assert!(recorder.seen().is_empty());
        });
    }

    #[test]
    fn published_work_pages_by_change_and_holds_only_this_sessions_items() {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let bystander = host
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let start = publisher.work_cursor().await.unwrap();
            let work = |receipt: PublishReceipt| receipt.queued[0].work.clone();
            let first = work(
                publish_as(&publisher, &SendOrigin::Session, 0)
                    .await
                    .unwrap(),
            );
            publish_as(&bystander, &SendOrigin::Session, 0)
                .await
                .unwrap();
            let second = work(
                publish_as(&publisher, &SendOrigin::Session, 1)
                    .await
                    .unwrap(),
            );
            let page = publisher.published_work_since(start, SINGLE).await.unwrap();
            assert_eq!(names(&page), [first.as_str()]);
            let rest = publisher
                .published_work_since(page[0].0, PAGE)
                .await
                .unwrap();
            assert_eq!(names(&rest), [second.as_str()]);
            let latest = rest[0].0;
            assert!(page[0].0 < latest);
            let none = publisher.published_work_since(latest, PAGE).await.unwrap();
            assert!(none.is_empty());
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            worker.claim_handle().unwrap();
            assert!(worker.acquire_work().await.unwrap());
            let changed = publisher.published_work_since(latest, PAGE).await.unwrap();
            assert_eq!(names(&changed), [first.as_str()]);
            let (cursor, item) = &changed[0];
            assert!(*cursor > latest);
            assert_eq!(publisher.work_cursor().await.unwrap(), *cursor);
            assert_eq!(
                (
                    item.group.as_str(),
                    &item.state,
                    item.attempts,
                    item.max_attempts
                ),
                (GROUP, &WorkState::Leased, 1, DEFAULT_MAX_ATTEMPTS)
            );
            assert_eq!(item.owner.as_ref().unwrap().handle, worker.handle());
            assert_eq!(
                item.message.message.audience,
                MessageAudience::Topic(TOPIC.into())
            );
        });
    }

    /// How the turn that took work in ends, or what a person does to it.
    enum Ending {
        Turn(Option<DoneReason>),
        Close,
        Person,
    }

    #[test_case(Ending::Turn(Some(DoneReason::EndTurn)), WorkPause::CompletionRequired; "completion_required")]
    #[test_case(Ending::Turn(Some(DoneReason::Cancelled)), WorkPause::Cancelled; "cancelled")]
    #[test_case(Ending::Turn(Some(DoneReason::MaxTurns)), WorkPause::TurnLimit; "turn_limit")]
    #[test_case(Ending::Turn(None), WorkPause::TurnFailed; "turn_failed")]
    #[test_case(Ending::Close, WorkPause::SessionClosed; "session_closed")]
    #[test_case(Ending::Person, WorkPause::Manual; "paused_by_a_person")]
    fn work_reports_name_the_pause_that_stopped_the_work(ending: Ending, pause: WorkPause) {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            match ending {
                Ending::Turn(done) => {
                    start(&worker).await;
                    worker.settle_work(done).await;
                }
                Ending::Close => {
                    start(&worker).await;
                    worker.close();
                }
                Ending::Person => {
                    worker.manage_work(&work, WorkAction::Pause).await.unwrap();
                }
            }
            let paused = WorkReported {
                group: GROUP.into(),
                work,
                outcome: ReportedOutcome::Paused,
                pause: Some(pause),
                detail: None,
            };
            assert_eq!(worker.take_work_reports(), [paused]);
            assert!(worker.take_work_reports().is_empty());
        });
    }

    #[test_case(WorkOutcome::Completed(Some(SUMMARY.into())), ReportedOutcome::Completed, SUMMARY; "completed")]
    #[test_case(WorkOutcome::Retry(FAILURE.into()), ReportedOutcome::Retry, FAILURE; "retried")]
    #[test_case(WorkOutcome::Failed(FAILURE.into()), ReportedOutcome::Failed, FAILURE; "failed")]
    fn work_reports_carry_the_outcome_the_agent_reported(
        outcome: WorkOutcome,
        reported: ReportedOutcome,
        detail: &str,
    ) {
        smol::block_on(async {
            let (directory, host, publisher) = grouped().await;
            let worker = member(&host, directory.path(), InboundPolicy::Auto);
            let work = publish(&publisher).await;
            start(&worker).await;
            worker.report_work(&work, outcome).await.unwrap();
            let expected = WorkReported {
                group: GROUP.into(),
                work,
                outcome: reported,
                pause: None,
                detail: Some(detail.into()),
            };
            assert_eq!(worker.take_work_reports(), [expected]);
        });
    }

    /// What leaves consumer groups no room for a publication.
    enum Full {
        Backlog,
        Outstanding,
        Fanout,
        Destinations,
    }

    /// Queues work in `GROUP` and `MAX_GROUPS - 1` more groups until one more
    /// publication would exceed the outstanding limit.
    async fn fill_outstanding(publisher: &PeerSession) {
        for index in 1..MAX_GROUPS {
            let name = format!("{OTHER_GROUP}-{index}");
            create_group(publisher, &name, GroupPolicy::default()).await;
        }
        let sender = lock(&publisher.0.state)
            .sender(&publisher.0.route, &SendOrigin::Session)
            .history_entry();
        let messages: Vec<NewMessage> = (0..MAX_OUTSTANDING / MAX_GROUPS)
            .map(|_| NewMessage {
                message_id: token().unwrap(),
                audience: MessageAudience::Topic(TOPIC.into()),
                sender: sender.clone(),
                text: TEXT.into(),
                reply_to: None,
                created_ms: wall_ms(),
            })
            .collect();
        publisher
            .0
            .host
            .history
            .query(move |log| {
                messages.iter().try_for_each(|message| {
                    log.record_publication(message, &[], usize::MAX).map(drop)
                })
            })
            .await
            .unwrap();
    }

    async fn group_full(full: Full) -> (SendFailure, String) {
        let directory = directory();
        let host = host(directory.path());
        let max_fanout = match full {
            Full::Fanout | Full::Destinations => SINGLE,
            Full::Backlog | Full::Outstanding => MessagingConfig::default().max_fanout,
        };
        let messaging = MessagingConfig {
            max_fanout,
            ..MessagingConfig::default()
        };
        let publisher = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Auto),
                &messaging,
                None,
            )
            .unwrap();
        let automation = SendOrigin::Automation(AUTOMATION.into());
        let single_backlog = GroupPolicy {
            max_backlog: SINGLE_BACKLOG,
            ..GroupPolicy::default()
        };
        match full {
            Full::Backlog => {
                create_group(&publisher, GROUP, single_backlog).await;
                publish_as(&publisher, &automation, 0).await.unwrap();
                let failure = publish_as(&publisher, &automation, 1).await.unwrap_err();
                (failure, WorkRefusal::BacklogFull(GROUP.into()).to_string())
            }
            Full::Outstanding => {
                create_group(&publisher, GROUP, GroupPolicy::default()).await;
                fill_outstanding(&publisher).await;
                let failure = publish_as(&publisher, &automation, 0).await.unwrap_err();
                (failure, WorkRefusal::OutstandingFull.to_string())
            }
            Full::Fanout => {
                create_group(&publisher, GROUP, single_backlog).await;
                publish_as(&publisher, &automation, 0).await.unwrap();
                publish_as(&publisher, &automation, 1).await.unwrap_err();
                create_group(&publisher, OTHER_GROUP, GroupPolicy::default()).await;
                let failure = publish_as(&publisher, &automation, 1).await.unwrap_err();
                let refusal = WorkRefusal::GroupFanout {
                    groups: BOTH_GROUPS.len(),
                    room: SINGLE,
                };
                (failure, refusal.to_string())
            }
            Full::Destinations => {
                for group in BOTH_GROUPS {
                    create_group(&publisher, group, GroupPolicy::default()).await;
                }
                let failure = publish_as(&publisher, &automation, 0).await.unwrap_err();
                (
                    failure,
                    recipient_room(BOTH_GROUPS.len(), SINGLE).unwrap_err(),
                )
            }
        }
    }

    #[test_case(Full::Backlog; "backlog_full")]
    #[test_case(Full::Outstanding; "outstanding_full")]
    #[test_case(Full::Fanout; "group_fanout")]
    #[test_case(Full::Destinations; "more_groups_than_destinations")]
    fn publications_consumer_groups_have_no_room_for_fail_as_group_full(full: Full) {
        let (failure, reason) = smol::block_on(group_full(full));
        assert_eq!(
            (failure.kind, failure.reason),
            (SendFailureKind::GroupFull, reason)
        );
    }
}
