//! The session's side of its automation runtime: what the runtime hears from this session, the
//! busy-period tracker behind `idle`, the `next` deliveries the session starts, and the controls
//! its saves keep. `AgentHandles` owns the runtime; the link holds a handle, which still reads
//! the final mirror once the runtime has stopped, so the last save keeps its counters.
//!
//! It also hosts `/automations`: the inspector, the chip, and the failure flash. Every request
//! goes out from a task of its own and its answer lands on a later tick, so the loop never
//! waits on the runtime.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use caudra_agent::GoalResult;
use caudra_agent::automation::busy::pause_reason;
pub(crate) use caudra_agent::automation::frontend::InputWait;
pub(super) use caudra_agent::automation::frontend::run_end;
use caudra_agent::automation::frontend::{
    SessionSignals, claim_next, delivery_due, goal_finished, mode_name, needs_input,
    profile_armings, saved_controls, set_claimed_goal,
};
use caudra_agent::automation::handle::AutomationHandle;
use caudra_agent::automation::outbox::claim_message;
use caudra_agent::peers::{AssignedWork, WorkPause, WorkReported, WorkState};
use caudra_automation::event::{
    GoalView, HeldWork, InputKind, PausedWork, SessionStatus, SessionView, StartedBy, TurnOutcome,
    WorkView,
};
use caudra_automation::request::{
    AutomationError, AutomationRequest, AutomationResponse, OutboxClaim, SessionSignal,
};
use caudra_automation::snapshot::{
    ArmOrigin, AutomationEvent, FiringStatus, FiringSummary, PauseSource, SettleBlocker,
};
use caudra_automation::untrusted::Untrusted;
use caudra_config::{Feature, ToolKey};
use caudra_providers::{AutomationEventOrigin, Message, PeerMessageOrigin};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::StoredAutomationControls;
use serde_json::Value;
use tracing::{debug, warn};

use super::{AUTOMATION_NOTICE_PREFIX, App, PendingInput};
use crate::components::automation_inspector::AutomationAction;
use crate::components::document_view::COPIED_SELECTION;
use crate::components::status_bar::AutomationChip;
use crate::components::{Action, Overlay, Status, escape_terminal_controls};
use crate::repaint::{Cadence, Dirty};

pub(super) const SAVE_FAILED: &str = "Automations need this session saved, and saving it failed";
pub(super) const CLAIM_FAILED: &str = "An automation delivery could not be claimed";
pub(super) const UNAVAILABLE_MSG: &str = "Automations are unavailable in this session";
pub(super) const AUTOMATIONS_USAGE: &str = "Usage: /automations [arm NAME [{json}] | disarm NAME]";
pub(super) const ARGS_NOT_JSON: &str = "Automation args are not valid JSON: ";
pub(super) const ARMED_PREFIX: &str = "Armed ";
pub(super) const DISARMED_PREFIX: &str = "Disarmed ";
pub(super) const ARMS_ONCE_SAVED: &str = " arms once this session is saved";
pub(super) const ARM_WORD: &str = "arm";
pub(super) const DISARM_WORD: &str = "disarm";
pub(super) const FAILED_AT_LINE: &str = " failed at line ";
const FAILED: &str = " failed";
pub(super) const INSPECTOR_HINT: &str = " (/automations)";
/// How often one automation's failures may flash.
const FAILURE_FLASH_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(test)]
const REPLY_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REPLY_MISSING: &str = "the automation runtime must answer a request in flight";

/// The session's side of its runtime: what it last told it, shared with every frontend, and
/// what only the TUI keeps.
#[derive(Default)]
pub(crate) struct AutomationLink {
    handle: Option<AutomationHandle>,
    signals: SessionSignals,
    /// The consumer groups the session takes work from, as its peer registration last had them.
    groups: Vec<String>,
    /// The item a run took in, and the paused items the history last answered with, both moved
    /// at once by each outcome the session reports.
    work: WorkView,
    replies: Replies,
    /// Failed firings since the inspector was last opened, for the chip.
    unseen_failures: usize,
    /// When each automation's failure last flashed.
    failure_flashes: HashMap<String, Instant>,
}

/// Where an answer lands: in the inspector, or as a flash for the command that asked.
enum ReplyTo {
    Inspector,
    Command,
}

/// The runtime's answer to `request`, from the session whose runtime gave it.
struct AutomationReply {
    session_id: CaudraId,
    to: ReplyTo,
    request: AutomationRequest,
    result: Result<AutomationResponse, AutomationError>,
}

/// The channel answers come back through, drained each tick.
struct Replies {
    tx: flume::Sender<AutomationReply>,
    rx: flume::Receiver<AutomationReply>,
}

impl Default for Replies {
    fn default() -> Self {
        let (tx, rx) = flume::unbounded();
        Self { tx, rx }
    }
}

impl Replies {
    /// Whether a request is in flight: its task holds a sender until it has answered.
    fn awaited(&self) -> bool {
        self.rx.sender_count() > 1
    }
}

impl AutomationLink {
    pub(crate) fn new(handle: AutomationHandle, facts: SessionView) -> Self {
        Self {
            handle: Some(handle),
            signals: SessionSignals::new(facts),
            ..Self::default()
        }
    }

    /// Whether `automation`'s failure may flash at `now`: once a minute at most, each.
    fn failure_flash_due(&mut self, automation: &str, now: Instant) -> bool {
        if self
            .failure_flashes
            .get(automation)
            .is_some_and(|last| now.duration_since(*last) < FAILURE_FLASH_INTERVAL)
        {
            return false;
        }
        self.failure_flashes.insert(automation.to_owned(), now);
        true
    }
}

impl App {
    /// The runtime serving this session, never one a session change left behind.
    fn automation_handle(&self) -> Option<&AutomationHandle> {
        self.automation
            .handle
            .as_ref()
            .filter(|handle| handle.session_id() == self.state.session.id)
    }

    pub(crate) fn has_automations(&self) -> bool {
        self.automation_handle().is_some()
    }

    fn signal_automations(&self, signal: SessionSignal) {
        if let Some(handle) = self.automation_handle() {
            handle.signal(signal);
        }
    }

    /// The session as automation events show it. `held_messages` and `name` come from the peer
    /// registration, as do the groups and paused work it last handed over; `since` is when the
    /// status began, in unix seconds.
    pub(crate) fn automation_facts(
        &self,
        held_messages: bool,
        name: Option<String>,
        since: i64,
    ) -> SessionView {
        SessionView {
            id: self.state.session.id.to_string(),
            title: Untrusted::text(self.state.session.title.clone()),
            name,
            mode: mode_name(&self.execution_agent_mode()).into(),
            status: self.automation_status(held_messages),
            status_since: since,
            goal: self.state.goal.snapshot().map(|goal| GoalView {
                condition: goal.condition.to_string(),
                evaluations: goal.evaluations,
            }),
            cost: self.state.cost,
            groups: self.automation.groups.clone(),
            work: self.automation.work.clone(),
        }
    }

    /// The consumer groups `session.groups` shows, as the peer registration has them now.
    pub(crate) fn set_automation_groups(&mut self, groups: Vec<String>) {
        self.automation.groups = groups;
    }

    /// Messaging is off: the session takes work from no group and holds or paused none it could
    /// still report on, and the inspector marks no session online.
    pub(crate) fn clear_automation_peer_facts(&mut self) {
        self.automation.groups.clear();
        self.automation.work = WorkView::default();
        self.automation_inspector.set_online(HashSet::new());
    }

    /// Whether the inspector is open, listing other sessions for the live peer directory to
    /// mark online.
    pub(crate) fn automation_inspector_open(&self) -> bool {
        self.automation_inspector.is_open()
    }

    /// The sessions the live peer directory lists, by id, which the inspector marks online.
    pub(crate) fn set_automation_online(&mut self, sessions: HashSet<String>) {
        self.automation_inspector.set_online(sessions);
    }

    /// The paused items `session.work` shows, of the work the history answered this session
    /// holds or paused. The item a run holds comes from the run instead.
    pub(crate) fn set_automation_paused_work(&mut self, items: &[AssignedWork]) {
        self.automation.work.paused = items.iter().filter_map(paused_work).collect();
    }

    /// The status peers see: held messages count as needing input, a ready plan does not.
    fn automation_status(&self, held_messages: bool) -> SessionStatus {
        if held_messages || self.awaiting_input() {
            SessionStatus::NeedsInput
        } else if self.status == Status::Streaming || self.waiting_for_background() {
            SessionStatus::Working
        } else {
            SessionStatus::Idle
        }
    }

    /// What the session waits on a person for, the run's own prompts first. A ready plan and
    /// held messages come last: neither stops a run.
    pub(crate) fn input_wait(&self, held_messages: bool) -> Option<InputWait> {
        let input = if self.permission_prompt.is_open() {
            return Some(InputWait {
                input: InputKind::Permission,
                tool: self
                    .permission_prompt
                    .tool()
                    .filter(|tool| !matches!(tool, ToolKey::Wildcard))
                    .map(|tool| tool.to_string()),
            });
        } else if self.question_form.is_open() {
            InputKind::Question
        } else if self.pending_input != PendingInput::None {
            InputKind::Auth
        } else if self.float_mgr.needs_input() {
            InputKind::Plugin
        } else if self.status != Status::Streaming && self.plan_form_active() {
            InputKind::Plan
        } else if held_messages {
            InputKind::Messages
        } else {
            return None;
        };
        Some(InputWait { input, tool: None })
    }

    /// The mirror already holds what an event announces: an open inspector re-reads it, and the
    /// chip follows it.
    pub(crate) fn handle_automation_event(&mut self, event: AutomationEvent) -> Dirty {
        match event {
            AutomationEvent::SaveSession => self.save_for_automations(),
            AutomationEvent::Notice {
                automation, text, ..
            } => self.flash(format!(
                "{AUTOMATION_NOTICE_PREFIX} {}: {}",
                escape_terminal_controls(&automation),
                escape_terminal_controls(&text)
            )),
            AutomationEvent::Firing { firing, .. } => {
                if firing.status == FiringStatus::Failed {
                    self.automation_failed(&firing);
                }
                self.sync_automation_inspector();
            }
            AutomationEvent::Session(_)
            | AutomationEvent::Automation(_)
            | AutomationEvent::Outbox(_) => self.sync_automation_inspector(),
        }
        let _ = self.refresh_automation_chip();
        Dirty::YES
    }

    fn sync_automation_inspector(&mut self) {
        if !self.automation_inspector.is_open() {
            return;
        }
        let Some(state) = self.automation_handle().map(AutomationHandle::state) else {
            return;
        };
        for request in self.automation_inspector.sync(state) {
            self.request_automations(ReplyTo::Inspector, request);
        }
    }

    /// A failure counts on the chip until the inspector shows it, and flashes at most once a
    /// minute per automation.
    fn automation_failed(&mut self, firing: &FiringSummary) {
        if !self.automation_inspector.is_open() {
            self.automation.unseen_failures += 1;
        }
        if self
            .automation
            .failure_flash_due(&firing.automation, Instant::now())
        {
            self.flash(failure_flash(firing));
        }
    }

    /// `[auto · N]` counts what the mirror holds armed, and the failures nobody has looked at.
    fn refresh_automation_chip(&mut self) -> Dirty {
        let armed = self.automation_handle().map_or(0, |handle| {
            handle
                .state()
                .automations
                .iter()
                .filter(|automation| automation.armed.is_some())
                .count()
        });
        self.status_bar.set_automations(AutomationChip {
            armed,
            unseen_failures: self.automation.unseen_failures,
        })
    }

    /// Lands the runtime's answers, asks again for what the inspector reads on the clock, and
    /// keeps the chip on the mirror: a session change replaces the runtime without an event to
    /// say so.
    pub(super) fn poll_automations(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Ok(reply) = self.automation.replies.rx.try_recv() {
            self.land_automation_reply(reply);
            dirty = Dirty::YES;
        }
        self.reread_automations(Instant::now());
        dirty | self.refresh_automation_chip()
    }

    /// Sends what the inspector reads again at `now`: another session's selected automation.
    fn reread_automations(&mut self, now: Instant) {
        if let Some(request) = self.automation_inspector.poll(now) {
            self.request_automations(ReplyTo::Inspector, request);
        }
    }

    /// A request in flight is looked at on the pending frame, so its answer lands promptly
    /// rather than on the inspector's one-second clock.
    pub(super) fn automation_cadence(&self) -> Cadence {
        Cadence::when(self.automation.replies.awaited(), Cadence::PENDING)
    }

    /// Blocks until the runtime answers a request in flight, lands the answer, and hands back
    /// the request it answered.
    #[cfg(test)]
    pub(crate) fn await_automation_reply(&mut self) -> AutomationRequest {
        let reply = self
            .automation
            .replies
            .rx
            .recv_timeout(REPLY_WAIT)
            .expect(REPLY_MISSING);
        let request = reply.request.clone();
        self.land_automation_reply(reply);
        request
    }

    /// Sends `request` from a task of its own; the answer comes back through the replies.
    fn request_automations(&mut self, to: ReplyTo, request: AutomationRequest) {
        let Some(handle) = self.automation_handle().cloned() else {
            self.flash(UNAVAILABLE_MSG.into());
            return;
        };
        let replies = self.automation.replies.tx.clone();
        smol::spawn(async move {
            let result = handle.request(request.clone()).await;
            let _ = replies.send(AutomationReply {
                session_id: handle.session_id(),
                to,
                request,
                result,
            });
        })
        .detach();
    }

    fn land_automation_reply(&mut self, reply: AutomationReply) {
        if reply.session_id != self.state.session.id {
            debug!(
                reply_session = %reply.session_id,
                session_id = %self.state.session.id,
                "automation answer for a session this tab has left dropped"
            );
            return;
        }
        match reply.to {
            ReplyTo::Inspector => {
                let action = self
                    .automation_inspector
                    .apply_response(&reply.request, reply.result);
                self.handle_automation_action(action);
            }
            ReplyTo::Command => self.report_automation_command(&reply.request, reply.result),
        }
    }

    /// What `/automations arm` or `disarm` came to, in one short flash.
    fn report_automation_command(
        &mut self,
        request: &AutomationRequest,
        result: Result<AutomationResponse, AutomationError>,
    ) {
        let snapshot = match result {
            Ok(AutomationResponse::Automation(snapshot)) => snapshot,
            Ok(response) => {
                warn!(?request, ?response, "unexpected automation reply");
                return;
            }
            Err(error) => {
                self.flash(escape_terminal_controls(&error.to_string()));
                return;
            }
        };
        let name = escape_terminal_controls(&snapshot.name);
        self.flash(match (request, snapshot.armed) {
            (AutomationRequest::Disarm { .. }, _) => format!("{DISARMED_PREFIX}{name}"),
            (_, Some(_)) => format!("{ARMED_PREFIX}{name}"),
            (_, None) => format!("{name}{ARMS_ONCE_SAVED}"),
        });
    }

    /// `/automations`: the inspector bare, or an arming by name whose answer flashes.
    pub(super) fn execute_automations(&mut self, args: &str) -> Vec<Action> {
        match parse_automations_command(args) {
            Ok(None) => self.open_automation_inspector(None),
            Ok(Some(request)) => self.request_automations(ReplyTo::Command, request),
            Err(message) => self.flash(message),
        }
        Vec::new()
    }

    /// Opens the inspector on the mirror, on `focus` when it names a script or a firing. Opening
    /// shows the failures the chip counted.
    pub(super) fn open_automation_inspector(&mut self, focus: Option<&str>) {
        if self.refuse_disabled(Feature::Automations) {
            return;
        }
        let Some(state) = self.automation_handle().map(AutomationHandle::state) else {
            self.flash(UNAVAILABLE_MSG.into());
            return;
        };
        self.automation.unseen_failures = 0;
        for request in self.automation_inspector.open(state, focus) {
            self.request_automations(ReplyTo::Inspector, request);
        }
        let _ = self.refresh_automation_chip();
    }

    pub(super) fn handle_automation_action(&mut self, action: AutomationAction) {
        match action {
            AutomationAction::None | AutomationAction::Passthrough => {}
            AutomationAction::Close => self.automation_inspector.close(),
            AutomationAction::Request(request) => {
                self.request_automations(ReplyTo::Inspector, request);
            }
            AutomationAction::OpenScript { path, line } => {
                self.automation_inspector.close();
                let lines = line.map(|line| line as usize..=line as usize);
                self.open_workbench_file(&path, lines);
            }
            AutomationAction::Copy(text) => self.copy_to_clipboard(&text),
            AutomationAction::Cut { text, request } => {
                self.copy_labelled(&text, COPIED_SELECTION);
                if let Some(request) = request {
                    self.request_automations(ReplyTo::Inspector, request);
                }
            }
            AutomationAction::Flash(message) => self.flash(escape_terminal_controls(&message)),
            AutomationAction::OpenWorkflowRun(run_id) => {
                self.automation_inspector.close();
                self.open_workflow_inspector(Some(&run_id));
            }
        }
    }

    /// The runtime writes nothing until the session has its row.
    fn save_for_automations(&mut self) {
        match self.publish_conversation_permissions() {
            Ok(()) => self.signal_automations(SessionSignal::Saved),
            Err(error) => self.flash(format!("{SAVE_FAILED}: {error}")),
        }
    }

    /// Brings the runtime up to date with what the event loop sees each tick: the facts its
    /// events carry, what the session waits on, and whether it settled. `blockers` is empty
    /// once the session has settled.
    pub(crate) fn observe_automations(
        &mut self,
        held_messages: bool,
        name: Option<String>,
        blockers: Vec<SettleBlocker>,
    ) {
        let Some(now) = self.automation_handle().map(AutomationHandle::now_ms) else {
            return;
        };
        self.sync_automation_facts(held_messages, name, now);
        let wait = self.input_wait(held_messages);
        let signals = [
            self.automation.signals.wait(wait),
            self.automation.signals.settle(blockers, now),
        ];
        for signal in signals.into_iter().flatten() {
            self.signal_automations(signal);
        }
    }

    fn sync_automation_facts(&mut self, held_messages: bool, name: Option<String>, now: i64) {
        self.automation
            .signals
            .observe_goal(self.state.goal.snapshot());
        let since = self
            .automation
            .signals
            .status_since(self.automation_status(held_messages), now);
        let facts = self.automation_facts(held_messages, name, since);
        if let Some(signal) = self.automation.signals.facts(facts) {
            self.signal_automations(signal);
        }
    }

    /// Someone at the terminal is working through the open question, so its `after` delays
    /// start over.
    pub(super) fn renew_question_wait(&self) {
        if let Some(wait) = self.automation.signals.input()
            && wait.input == InputKind::Question
        {
            self.signal_automations(needs_input(wait));
        }
    }

    /// The user stopped the main run: every automation of the session holds until they type.
    pub(super) fn pause_automations(&self) {
        self.signal_automations(SessionSignal::Pause {
            by: PauseSource::User,
        });
    }

    /// A person sent a prompt: the runtime resets its unattended count and backoff and clears
    /// the pause latch.
    pub(super) fn human_input(&self) {
        self.signal_automations(SessionSignal::HumanInput);
    }

    pub(crate) fn sync_automation_profile(&self) {
        self.signal_automations(SessionSignal::ProfileAutomations(profile_armings(
            self.state.system_prompt_profile.as_deref(),
        )));
    }

    pub(super) fn automation_run_started(&mut self, started_by: StartedBy) {
        let Some(now) = self.automation_handle().map(AutomationHandle::now_ms) else {
            return;
        };
        self.automation
            .signals
            .run_started(started_by, now, self.state.cost);
    }

    /// A queued prompt the agent picked up on its own starts a run when none is in flight, as
    /// a restored queue or a follow-up after the last run does.
    pub(super) fn automation_queue_consumed(&mut self) {
        if !self.automation.signals.running() {
            self.automation_run_started(StartedBy::User);
        }
    }

    pub(super) fn automation_injected(&mut self, origin: &AutomationEventOrigin) {
        if self.has_automations() {
            self.automation.signals.injected(origin);
        }
    }

    /// A run took in a peer message, which may name who started the busy period. The work it
    /// assigns is the item the session holds: taking it in changes nothing the history's version
    /// would show.
    pub(super) fn automation_peer_injected(&mut self, origin: &PeerMessageOrigin) {
        if !self.has_automations() {
            return;
        }
        self.automation.signals.busy.peer_injected(origin);
        if let Some(assignment) = &origin.assignment {
            self.automation.work.held = Some(HeldWork {
                group: assignment.group.clone(),
                work: assignment.work.clone(),
                attempt: assignment.attempt,
                max_attempts: assignment.max_attempts,
            });
        }
    }

    /// What became of the session's group work. The busy period records it, and `session.work`
    /// follows at once rather than once the history answers again, so the `idle` the period
    /// settles into already shows it.
    pub(crate) fn automation_work_reported(&mut self, reports: Vec<WorkReported>) {
        if !self.has_automations() {
            return;
        }
        for report in reports {
            follow_report(&mut self.automation.work, &report);
            self.automation.signals.busy.work_reported(report);
        }
    }

    pub(super) fn automation_turn_complete(&mut self, message: &Message) {
        self.automation.signals.turn_complete(message);
    }

    /// The agent pauses work a run took in and reported nothing for before the run ends, so the
    /// session holds none once it has.
    pub(super) fn automation_run_ended(&mut self, outcome: TurnOutcome, error: Option<String>) {
        if !self.has_automations() {
            return;
        }
        self.automation.work.held = None;
        let signal = self
            .automation
            .signals
            .run_ended(outcome, error, self.state.cost);
        self.signal_automations(signal);
    }

    pub(super) fn automation_goal_finished(&self, result: &GoalResult) {
        self.signal_automations(goal_finished(result));
    }

    pub(super) fn automation_goal_cleared(&self, condition: &str, message: &str) {
        if self.has_automations() {
            self.signal_automations(self.automation.signals.goal_cleared(condition, message));
        }
    }

    /// Whether a `next` item could start a turn now as far as this session knows: it is idle
    /// with no modal up, a turn would be admitted, the runtime heard it settle, so a claim never
    /// joins the period that settle closes, and an item waits on nothing that still holds.
    /// Suppressed automatic wakes do not hold it: the runtime's own latch and limits do.
    pub(crate) fn automation_delivery_due(&self) -> bool {
        let Some(handle) = self.automation_handle() else {
            return false;
        };
        self.status == Status::Idle
            && !self.has_modal_overlay()
            && self.check_run_admission().is_ok()
            && self.automation.signals.settled()
            && delivery_due(handle)
    }

    /// Starts the turn the next `next` item asks for. The turn is admitted before the claim,
    /// because the runtime records an item delivered as it hands it over.
    pub(crate) fn deliver_automation_item(&mut self) -> Option<Vec<Action>> {
        if let Err(error) = self.admit_run() {
            self.flash(error);
            return None;
        }
        let claim = self.claim_automation_item()?;
        Some(self.start_automation_run(claim))
    }

    /// Claims the next item for a turn. The runtime records it delivered before answering, so
    /// the caller has already checked everything that could stop that turn. A runtime that is
    /// stopping answers `Unavailable` and keeps its items, which is no failure to show.
    fn claim_automation_item(&mut self) -> Option<OutboxClaim> {
        let handle = self.automation_handle()?.clone();
        match smol::block_on(claim_next(&handle)) {
            Ok(claim) => claim,
            Err(AutomationError::Unavailable) => {
                debug!(
                    session_id = %self.state.session.id,
                    "automation runtime stopped before its delivery claim"
                );
                None
            }
            Err(error) => {
                warn!(
                    %error,
                    session_id = %self.state.session.id,
                    "automation delivery claim failed"
                );
                self.flash(format!("{CLAIM_FAILED}: {error}"));
                None
            }
        }
    }

    /// Starts the turn a claimed item asked for. Unlike a mailbox wake it ignores
    /// `automatic_wakes_suppressed`, so deliveries go on after an error: the runtime's latch,
    /// turn rate, cap and backoff already let it through.
    pub(super) fn start_automation_run(&mut self, claim: OutboxClaim) -> Vec<Action> {
        if let Some(goal) = &claim.goal
            && let Some(refusal) = set_claimed_goal(&self.state.goal, goal)
        {
            self.flash(format!(
                "{AUTOMATION_NOTICE_PREFIX} {}: {refusal}",
                escape_terminal_controls(&claim.automation)
            ));
        }
        let mut input = self.continuation_input();
        input.preamble.push(claim_message(&claim));
        let OutboxClaim {
            automation,
            fire_id,
            ..
        } = claim;
        self.start_admitted_run(
            input,
            String::new(),
            StartedBy::Automation {
                automation,
                fire_id,
            },
        )
    }

    /// What `SessionMeta.automations` keeps while a runtime serves this session; `None` leaves
    /// the stored value as it is.
    pub(super) fn stored_automation_controls(&self) -> Option<StoredAutomationControls> {
        self.automation_handle().map(saved_controls)
    }
}

/// `/automations [arm NAME [{json}] | disarm NAME]`: `None` opens the inspector. The args are
/// the rest of the line after the name, so their JSON may hold spaces.
fn parse_automations_command(args: &str) -> Result<Option<AutomationRequest>, String> {
    let args = args.trim();
    if args.is_empty() {
        return Ok(None);
    }
    let (word, rest) = split_word(args);
    let (name, rest) = split_word(rest.trim_start());
    let rest = rest.trim();
    match word {
        ARM_WORD if !name.is_empty() => {
            let args = (!rest.is_empty())
                .then(|| serde_json::from_str::<Value>(rest))
                .transpose()
                .map_err(|error| format!("{ARGS_NOT_JSON}{error}"))?;
            Ok(Some(AutomationRequest::Arm {
                name: name.to_owned(),
                args,
                origin: ArmOrigin::Manual,
            }))
        }
        DISARM_WORD if !name.is_empty() && rest.is_empty() => Ok(Some(AutomationRequest::Disarm {
            name: name.to_owned(),
        })),
        _ => Err(AUTOMATIONS_USAGE.to_owned()),
    }
}

fn split_word(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace).unwrap_or((text, ""))
}

/// `goal-chain failed at line 12 (/automations)`, without the line when the error has none.
fn failure_flash(firing: &FiringSummary) -> String {
    let name = escape_terminal_controls(&firing.automation);
    match firing.error.as_ref().and_then(|error| error.line) {
        Some(line) => format!("{name}{FAILED_AT_LINE}{line}{INSPECTOR_HINT}"),
        None => format!("{name}{FAILED}{INSPECTOR_HINT}"),
    }
}

/// `item` as `session.work` lists a paused item. An item in any other state, or paused for a
/// reason this build cannot name, is none.
fn paused_work(item: &AssignedWork) -> Option<PausedWork> {
    if item.state != WorkState::Paused.as_str() {
        return None;
    }
    let pause = WorkPause::from_text(item.reason.as_deref()?)?;
    Some(PausedWork {
        group: item.group.clone(),
        work: item.work.clone(),
        pause_reason: pause_reason(pause),
    })
}

/// Moves `work` by what became of one item: the session holds it no longer, and a pause its own
/// run made lists it first among the paused. A person pauses queued items of any member through
/// `/groups`, so whether such an item is this session's is the history's to say.
fn follow_report(work: &mut WorkView, report: &WorkReported) {
    let reported = |group: &str, name: &str| group == report.group && name == report.work;
    if work
        .held
        .as_ref()
        .is_some_and(|held| reported(&held.group, &held.work))
    {
        work.held = None;
    }
    work.paused
        .retain(|paused| !reported(&paused.group, &paused.work));
    if let Some(pause) = report
        .pause
        .clone()
        .filter(|pause| *pause != WorkPause::Manual)
    {
        work.paused.insert(
            0,
            PausedWork {
                group: report.group.clone(),
                work: report.work.clone(),
                pause_reason: pause_reason(pause),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::AgentEvent;
    use caudra_agent::peers::{COMPLETION_REQUIRED, PAUSED_BY_CANCEL};
    use caudra_automation::event::{Audience, PauseReason, SenderKind, WorkOutcome, WorkReport};
    use caudra_automation::meta::TriggerKind;
    use caudra_automation::snapshot::ErrorView;
    use caudra_providers::{PeerAssignment, PeerAudience};
    use caudra_storage::messages::PAUSED_BY_USER;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::app::tests::automation_runtime::LinkedAutomations;
    use crate::app::tests::{agent_msg, test_app};
    use crate::components::command::ParsedCommand;

    const MILLIS_PER_SECOND: i64 = 1_000;
    const NOW: i64 = 1_790_000_000_000;
    const LATER: i64 = NOW + MILLIS_PER_SECOND;
    const AUTOMATION: &str = "courier";
    const OTHER_AUTOMATION: &str = "watchdog";
    const FIRE_ID: &str = "fire-1";
    const LINE: u32 = 12;
    const ERROR_KIND: &str = "runtime";
    const ERROR_MESSAGE: &str = "boom";
    const AUTOMATIONS_COMMAND: &str = "/automations";
    const ARGS: &str = r#"{"every":5}"#;
    const ARGS_WITH_SPACES: &str = r#"{ "repo": "my project", "every": 5 }"#;
    const BAD_JSON: &str = "{repo: mine}";
    const UNKNOWN_SUBCOMMAND: &str = "trust";
    const NOT_SETTLED: &str = "the settle after a run must report the period it closes";
    const SENDS_NOTHING: &str = "a malformed command must send no request";
    const SINCE: i64 = NOW / MILLIS_PER_SECOND;
    /// The run `agent_msg` events belong to.
    const RUN_ID: u64 = 1;
    const GROUP: &str = "ci-triage";
    const OTHER_GROUP: &str = "release";
    const WORK: &str = "quiet-amber-fox";
    const PAUSED: &str = "steady-teal-wren";
    const QUEUED: &str = "bright-cold-moth";
    const ATTEMPT: u32 = 2;
    const MAX_ATTEMPTS: u32 = 3;
    const MESSAGE_ID: &str = "brisk-calm-otter";
    const SENDER_NAME: &str = "Reviewer";
    const SENDER: &str = "@reviewer";
    const PUBLISHER: &str = "Publisher";
    const TOPIC: &str = "ci.failures";
    const PEER_TEXT: &str = "the linker step is flaky again";
    const UNKNOWN_REASON: &str = "paused by a later build";

    fn held_item(work: &str) -> HeldWork {
        HeldWork {
            group: GROUP.into(),
            work: work.into(),
            attempt: ATTEMPT,
            max_attempts: MAX_ATTEMPTS,
        }
    }

    fn paused_item(work: &str, pause_reason: PauseReason) -> PausedWork {
        PausedWork {
            group: GROUP.into(),
            work: work.into(),
            pause_reason,
        }
    }

    /// `work` as the history answers it for the session holding or pausing it.
    fn assigned_item(work: &str, state: WorkState, reason: Option<&str>) -> AssignedWork {
        AssignedWork {
            work: work.into(),
            group: GROUP.into(),
            state: state.as_str().into(),
            attempt: ATTEMPT,
            max_attempts: MAX_ATTEMPTS,
            topic: Some(TOPIC.into()),
            publisher: PUBLISHER.into(),
            reason: reason.map(str::to_owned),
            result: None,
        }
    }

    fn outcome(work: &str, outcome: WorkOutcome, pause: Option<WorkPause>) -> WorkReported {
        WorkReported {
            group: GROUP.into(),
            work: work.into(),
            outcome,
            pause,
            detail: None,
        }
    }

    /// A direct message from a session.
    fn peer_message() -> PeerMessageOrigin {
        PeerMessageOrigin {
            message_id: MESSAGE_ID.into(),
            audience: PeerAudience::Direct,
            sender_name: SENDER_NAME.into(),
            sender_handle: None,
            reply_target: SENDER.into(),
            reply_to: None,
            external: false,
            automation: None,
            assignment: None,
        }
    }

    /// The topic message that assigns `WORK`.
    fn assignment() -> PeerMessageOrigin {
        PeerMessageOrigin {
            audience: PeerAudience::Topic {
                topic: TOPIC.into(),
            },
            assignment: Some(PeerAssignment {
                group: GROUP.into(),
                work: WORK.into(),
                attempt: ATTEMPT,
                max_attempts: MAX_ATTEMPTS,
            }),
            ..peer_message()
        }
    }

    fn started_by_peer() -> StartedBy {
        StartedBy::Peer {
            message_id: MESSAGE_ID.into(),
            sender: Some(SENDER.into()),
            sender_kind: SenderKind::Session,
            audience: Audience::Direct,
            topic: None,
        }
    }

    fn started_by_work() -> StartedBy {
        StartedBy::Work {
            group: GROUP.into(),
            work: WORK.into(),
            attempt: ATTEMPT,
            max_attempts: MAX_ATTEMPTS,
            message_id: MESSAGE_ID.into(),
            topic: Some(TOPIC.into()),
        }
    }

    /// A session that holds `WORK` and paused `PAUSED` when a person stopped its run.
    fn holding_work() -> WorkView {
        WorkView {
            held: Some(held_item(WORK)),
            paused: vec![paused_item(PAUSED, PauseReason::Cancelled)],
        }
    }

    fn arm(args: Option<Value>) -> AutomationRequest {
        AutomationRequest::Arm {
            name: AUTOMATION.into(),
            args,
            origin: ArmOrigin::Manual,
        }
    }

    fn failed_firing(line: Option<u32>) -> FiringSummary {
        FiringSummary {
            fire_id: FIRE_ID.into(),
            automation: AUTOMATION.into(),
            digest: String::new(),
            trigger: TriggerKind::Armed,
            trigger_index: 0,
            event_key: None,
            consumed: false,
            status: FiringStatus::Failed,
            reason: None,
            error: Some(ErrorView {
                kind: ERROR_KIND.into(),
                message: ERROR_MESSAGE.into(),
                line,
                column: None,
            }),
            repeats: 1,
            attempts: 0,
            operations: 0,
            state_outcome: None,
            queued_at: NOW,
            deferred_until: None,
            started_at: None,
            finished_at: None,
            action_count: 0,
            first_action: None,
        }
    }

    #[test_case("", None ; "bare")]
    #[test_case(&format!("{ARM_WORD} {AUTOMATION}"), Some(arm(None)) ; "arm_without_args")]
    #[test_case(&format!("{ARM_WORD} {AUTOMATION} {ARGS}"), Some(arm(Some(json!({ "every": 5 })))) ; "arm_with_json")]
    #[test_case(
        &format!(" {ARM_WORD}  {AUTOMATION}  {ARGS_WITH_SPACES} "),
        Some(arm(Some(json!({ "repo": "my project", "every": 5 }))));
        "json_with_spaces"
    )]
    #[test_case(
        &format!("{DISARM_WORD} {AUTOMATION}"),
        Some(AutomationRequest::Disarm { name: AUTOMATION.into() });
        "disarm"
    )]
    fn the_command_reads_as_the_request_it_sends(line: &str, expected: Option<AutomationRequest>) {
        assert_eq!(parse_automations_command(line), Ok(expected));
    }

    #[test_case(&format!("{ARM_WORD} {AUTOMATION} {BAD_JSON}"), ARGS_NOT_JSON ; "bad_json")]
    #[test_case(&format!("{UNKNOWN_SUBCOMMAND} {AUTOMATION}"), AUTOMATIONS_USAGE ; "unknown_subcommand")]
    #[test_case(ARM_WORD, AUTOMATIONS_USAGE ; "arm_without_a_name")]
    #[test_case(&format!("{DISARM_WORD} {AUTOMATION} {ARGS}"), AUTOMATIONS_USAGE ; "disarm_with_args")]
    fn a_malformed_command_sends_nothing_and_says_why(line: &str, message: &str) {
        let error = parse_automations_command(line).expect_err(SENDS_NOTHING);
        assert!(error.starts_with(message), "{error}");
    }

    /// Without a runtime a well-formed command says so, so a parse error flashing instead
    /// shows nothing was sent.
    #[test_case("", UNAVAILABLE_MSG ; "bare")]
    #[test_case(&format!("{DISARM_WORD} {AUTOMATION}"), UNAVAILABLE_MSG ; "disarm")]
    #[test_case(&format!("{ARM_WORD} {AUTOMATION} {BAD_JSON}"), ARGS_NOT_JSON ; "bad_json")]
    fn without_a_runtime_the_command_says_why_it_did_nothing(args: &str, message: &str) {
        let mut app = test_app();

        let actions = app.execute_command(
            ParsedCommand {
                name: AUTOMATIONS_COMMAND.into(),
                args: args.into(),
            },
            0,
        );

        assert!(actions.is_empty());
        let flash = app.status_bar.flash_text().unwrap_or_default();
        assert!(flash.starts_with(message), "{flash}");
        assert!(!app.automation_inspector.is_open());
    }

    #[test]
    fn a_failure_flashes_at_most_once_a_minute_per_automation() {
        let mut link = AutomationLink::default();
        let first = Instant::now();
        let within = first + FAILURE_FLASH_INTERVAL / 2;

        assert!(link.failure_flash_due(AUTOMATION, first));
        assert!(!link.failure_flash_due(AUTOMATION, within));
        assert!(link.failure_flash_due(OTHER_AUTOMATION, within));
        assert!(link.failure_flash_due(AUTOMATION, first + FAILURE_FLASH_INTERVAL));
    }

    #[test_case(Some(LINE), &format!("{AUTOMATION}{FAILED_AT_LINE}{LINE}{INSPECTOR_HINT}") ; "at_its_line")]
    #[test_case(None, &format!("{AUTOMATION}{FAILED}{INSPECTOR_HINT}") ; "without_a_line")]
    fn a_failure_flash_names_the_automation_and_where_it_failed(line: Option<u32>, expected: &str) {
        assert_eq!(failure_flash(&failed_firing(line)), expected);
    }

    /// The loop polls replies on the pending frame while a task still holds a sender.
    #[test]
    fn a_request_in_flight_keeps_the_loop_looking_for_its_answer() {
        let app = test_app();
        assert_eq!(app.automation_cadence(), Cadence::IDLE);

        let in_flight = app.automation.replies.tx.clone();
        assert_eq!(app.automation_cadence(), Cadence::PENDING);

        drop(in_flight);
        assert_eq!(app.automation_cadence(), Cadence::IDLE);
    }

    #[test]
    fn the_facts_show_the_groups_and_the_work_the_session_holds_and_paused() {
        let mut app = test_app();
        app.automation.work.held = Some(held_item(WORK));

        app.set_automation_groups(vec![GROUP.into(), OTHER_GROUP.into()]);
        app.set_automation_paused_work(&[
            assigned_item(WORK, WorkState::Leased, None),
            assigned_item(PAUSED, WorkState::Paused, Some(PAUSED_BY_CANCEL)),
            assigned_item(QUEUED, WorkState::Paused, Some(PAUSED_BY_USER)),
        ]);

        let facts = app.automation_facts(false, None, SINCE);
        assert_eq!(facts.groups, [GROUP, OTHER_GROUP]);
        assert_eq!(
            facts.work,
            WorkView {
                held: Some(held_item(WORK)),
                paused: vec![
                    paused_item(PAUSED, PauseReason::Cancelled),
                    paused_item(QUEUED, PauseReason::Manual),
                ],
            }
        );
    }

    #[test_case(WorkState::Paused, Some(COMPLETION_REQUIRED), Some(PauseReason::CompletionRequired) ; "paused_by_its_run")]
    #[test_case(WorkState::Paused, Some(PAUSED_BY_USER), Some(PauseReason::Manual) ; "paused_by_a_person")]
    #[test_case(WorkState::Paused, Some(UNKNOWN_REASON), None ; "paused_for_a_reason_without_a_name")]
    #[test_case(WorkState::Pausing, Some(PAUSED_BY_CANCEL), None ; "still_pausing")]
    fn only_a_paused_item_with_a_named_reason_is_listed_as_paused(
        state: WorkState,
        reason: Option<&str>,
        expected: Option<PauseReason>,
    ) {
        assert_eq!(
            paused_work(&assigned_item(WORK, state, reason)),
            expected.map(|reason| paused_item(WORK, reason))
        );
    }

    #[test_case(
        outcome(WORK, WorkOutcome::Completed, None),
        None,
        &[(PAUSED, PauseReason::Cancelled)];
        "the_held_item_completes"
    )]
    #[test_case(
        outcome(WORK, WorkOutcome::Paused, Some(WorkPause::TurnLimit)),
        None,
        &[(WORK, PauseReason::TurnLimit), (PAUSED, PauseReason::Cancelled)];
        "the_held_item_pauses"
    )]
    #[test_case(outcome(PAUSED, WorkOutcome::Failed, None), Some(WORK), &[] ; "a_paused_item_fails")]
    #[test_case(
        outcome(QUEUED, WorkOutcome::Paused, Some(WorkPause::Manual)),
        Some(WORK),
        &[(PAUSED, PauseReason::Cancelled)];
        "a_person_pauses_a_queued_item"
    )]
    fn a_report_moves_the_work_the_facts_show(
        report: WorkReported,
        held: Option<&str>,
        paused: &[(&str, PauseReason)],
    ) {
        let mut work = holding_work();

        follow_report(&mut work, &report);

        assert_eq!(work.held, held.map(held_item));
        assert_eq!(
            work.paused,
            paused
                .iter()
                .map(|&(name, reason)| paused_item(name, reason))
                .collect::<Vec<_>>()
        );
    }

    #[test_case(peer_message(), started_by_peer(), None ; "a_peer_message")]
    #[test_case(assignment(), started_by_work(), Some(WORK) ; "a_work_assignment")]
    fn the_peer_message_a_mailbox_run_took_in_names_who_started_its_period(
        origin: PeerMessageOrigin,
        expected: StartedBy,
        held: Option<&str>,
    ) {
        let mut app = test_app();
        let linked = LinkedAutomations::courier(&mut app);
        app.status = Status::Streaming;
        app.run_id = RUN_ID;
        app.automation_run_started(StartedBy::Mailbox);

        app.update(agent_msg(AgentEvent::Injected {
            text: PEER_TEXT.into(),
            task_event: None,
            peer_event: Some(origin),
            automation_event: None,
        }));

        assert_eq!(
            app.automation_facts(false, None, SINCE).work.held,
            held.map(held_item)
        );
        let Some(idle) = app.automation.signals.busy.settle(LATER) else {
            panic!("{NOT_SETTLED}");
        };
        assert_eq!(idle.started_by, expected);
        linked.stop();
    }

    #[test]
    fn a_reported_outcome_reaches_the_period_and_the_facts_at_once() {
        let mut app = test_app();
        let linked = LinkedAutomations::courier(&mut app);
        app.automation_run_started(StartedBy::Mailbox);
        app.automation_peer_injected(&assignment());
        let paused = outcome(WORK, WorkOutcome::Paused, Some(WorkPause::TurnLimit));

        app.automation_work_reported(vec![paused.clone(), paused]);

        assert_eq!(
            app.automation_facts(false, None, SINCE).work,
            WorkView {
                held: None,
                paused: vec![paused_item(WORK, PauseReason::TurnLimit)],
            }
        );
        let Some(idle) = app.automation.signals.busy.settle(LATER) else {
            panic!("{NOT_SETTLED}");
        };
        assert_eq!(
            idle.work,
            [WorkReport {
                group: GROUP.into(),
                work: WORK.into(),
                outcome: WorkOutcome::Paused,
                pause_reason: Some(PauseReason::TurnLimit),
                detail: None,
            }]
        );
        linked.stop();
    }
}
