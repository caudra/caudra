use std::collections::HashMap;

use caudra_agent::peers::topics::{add_patterns, remove_patterns};
use caudra_agent::peers::{
    PeerDecision, PeerDecisionResult, PeerDescriptor, PeerReviewToken, PeerSession, PeerSummary,
};
use caudra_config::{Feature, InboundPolicy};
use flume::{Receiver, TryRecvError};
use smol::Task;

use super::{EventLoop, SessionRuntime, SessionStatus, SpawnCtx};
use crate::AppSession;
use crate::app::App;
use crate::components::{DisplayMessage, DisplayRole, ExitRequest, Status, peer_manager::PeerView};
use crate::repaint::Dirty;

const UNAVAILABLE: &str = "Cross-session messaging is unavailable for this runtime";
const REVIEW_REQUIRED: &str =
    "Message review expired or was not opened; run /messages and open that message's review";
const MESSAGES_HELP: &str = "Usage: /messages [help | approve ID | reject ID | inbound auto|accept|hold|refuse]\nRun /messages to open the Held messages view. Enter opens one message's review before approval or rejection. Approval accepts that message, not its requested tool actions, and can start a billable model turn. Inbound policy applies to this session and cannot relax project restrictions. Pending messages are live-only and expire when the runtime closes.";
const DISCOVERY_STOPPED: &str = "Peer discovery stopped before returning a result";
const MODAL_BLOCKED: &str = "Finish the pending session review before opening the peer manager";
const QUEUED: &str = "Message queued for the next safe boundary; an idle session may start after closing the manager";
const UNNAMED: &str =
    "this session continues without it and reclaims it when resumed while it is free";
const REJECTED: &str = "Message rejected and removed from the live inbox";
const TOPICS_HELP: &str = "Usage: /topics [help | subscribe PATTERN... | unsubscribe PATTERN... | broadcast on|off]\nRun /topics to show this session's subscriptions. A topic is a dot-separated name such as ci.failures. In a pattern, * matches one segment and a final ** matches one or more. Subscriptions decide which publications reach this session, up to 16 patterns. Broadcasts reach only sessions that turn them on. The inbound policy still decides whether each message is delivered, held, or refused. Subscriptions are kept with the session and restored when it resumes.";
const TOPICS_LABEL: &str = "Peer topics: ";
const NO_TOPICS: &str = "No peer topic subscriptions";
const BROADCASTS_ON: &str = "broadcasts on";
const BROADCASTS_OFF: &str = "broadcasts off";
const SUMMARY_SEPARATOR: &str = " · ";

type DiscoveryResult = Result<Vec<PeerSummary>, String>;

pub(super) struct PeerRegistration {
    session: PeerSession,
    reviewed: HashMap<String, PeerReviewToken>,
    held_count: usize,
    discovery: Option<PeerDiscovery>,
}

struct PeerDiscovery {
    generation: u64,
    receiver: Receiver<DiscoveryResult>,
    _task: Task<()>,
}

impl PeerRegistration {
    fn new(session: PeerSession) -> Self {
        Self {
            session,
            reviewed: HashMap::new(),
            held_count: 0,
            discovery: None,
        }
    }

    fn discover(&mut self, generation: u64) -> bool {
        if self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.generation == generation)
        {
            return false;
        }
        let (sender, receiver) = flume::bounded(1);
        let session = self.session.clone();
        self.discovery = Some(PeerDiscovery {
            generation,
            receiver,
            _task: smol::spawn(async move {
                let result = session.list_named().await;
                let _ = sender.try_send(result);
            }),
        });
        true
    }

    fn sync_manager(&mut self, app: &mut App) -> Dirty {
        let generation = app
            .peer_manager
            .is_open()
            .then(|| app.peer_manager.generation());
        let mut dirty = Dirty::NO;
        if let Some(result) = poll_discovery(&mut self.discovery, generation) {
            app.peer_manager.set_sessions(result);
            dirty = Dirty::YES;
        }
        if generation.is_some() {
            match self.session.inbox_snapshot() {
                Ok(snapshot) => {
                    self.reviewed.retain(|id, _| {
                        snapshot
                            .messages
                            .iter()
                            .any(|message| &message.message_id == id)
                    });
                    dirty |= Dirty::from(app.peer_manager.update_inbox(snapshot));
                }
                Err(error) => {
                    app.peer_manager.set_error(error);
                    dirty = Dirty::YES;
                }
            }
        }
        dirty
    }
}

fn poll_discovery(
    pending: &mut Option<PeerDiscovery>,
    generation: Option<u64>,
) -> Option<DiscoveryResult> {
    let discovery = pending.as_ref()?;
    if generation != Some(discovery.generation) {
        *pending = None;
        return None;
    }
    let result = match discovery.receiver.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Err(DISCOVERY_STOPPED.into()),
    };
    *pending = None;
    Some(result)
}

impl Drop for PeerRegistration {
    fn drop(&mut self) {
        self.session.close();
    }
}

fn same_controls(left: &PeerDescriptor, right: &PeerDescriptor) -> bool {
    left.session_id == right.session_id
        && left.cwd == right.cwd
        && left.mode == right.mode
        && left.permission_mode == right.permission_mode
        && left.inbound == right.inbound
        && left.blocked == right.blocked
}

fn peer_eligible(app: &App) -> bool {
    app.features.enabled(Feature::CrossSessionMessaging)
        && app.workspace_session.is_none()
        && app
            .state
            .session
            .workspace_binding()
            .is_none_or(|binding| binding.is_local())
        && app.sandbox_live.name.is_none()
}

fn peer_blocked(app: &App) -> bool {
    app.automatic_wakes_suppressed
        || app.cancelling_run.is_some()
        || app.awaiting_input()
        || app.lifecycle_blocker().is_some()
        || app.holds_recovery_text()
        || app.sandbox_network_dispatch_blocker().is_some()
        || app.state.session.meta.pending_revert.is_some()
        || app.exit_request != ExitRequest::None
        || matches!(app.status, Status::Error { .. })
}

fn decision_notice(result: &PeerDecisionResult) -> &'static str {
    match result {
        PeerDecisionResult::Queued => QUEUED,
        PeerDecisionResult::Rejected => REJECTED,
    }
}

impl SessionRuntime {
    pub(super) fn capture_peer_review(&mut self) {
        if let Some(peer) = &mut self.peer
            && let Some((id, token)) = self.app.peer_manager.reviewed_message()
        {
            peer.reviewed.insert(id.to_owned(), token.clone());
        }
    }

    pub(super) fn display_status(&self) -> SessionStatus {
        if self.peer.as_ref().is_some_and(|peer| peer.held_count > 0) {
            SessionStatus::NeedsInput
        } else {
            SessionStatus::of(&self.app)
        }
    }

    fn peer_blocked(&self) -> bool {
        peer_blocked(&self.app) || !self.restore_transitions.is_empty()
    }

    fn peer_descriptor(&self, inbound: InboundPolicy) -> PeerDescriptor {
        PeerDescriptor {
            session_id: self.id(),
            name: self.app.state.session.title.clone(),
            cwd: self.app.state.session.cwd.clone().into(),
            mode: self.app.execution_agent_mode(),
            permission_mode: self.app.permissions.mode(),
            inbound,
            blocked: self.peer_blocked(),
            busy: self.app.status == Status::Streaming || self.handles.queue.is_processing(),
        }
    }

    pub(super) fn install_peer(&mut self, ctx: &SpawnCtx) {
        if self.peer.is_some()
            || !ctx.config.features.enabled(Feature::CrossSessionMessaging)
            || !peer_eligible(&self.app)
        {
            return;
        }
        let Some(host) = &ctx.peer_host else {
            return;
        };
        match host.register_with_controls(
            self.peer_descriptor(ctx.config.messaging.inbound.clone()),
            &ctx.config.messaging,
            self.app.state.session.meta.peer_controls.clone(),
        ) {
            Ok(session) => {
                if let Err(error) = session.claim_handle() {
                    self.peer_notice(format!("{error}; {UNNAMED}"));
                }
                self.peer = Some(PeerRegistration::new(session));
            }
            Err(error) => self.app.flash(format!("{UNAVAILABLE}: {error}")),
        }
    }

    pub(super) fn close_peer(&mut self) {
        if let Some(peer) = &self.peer
            && peer.session.session_id() == self.id()
        {
            self.app.automatic_wakes_suppressed |= peer.session.wakes_suppressed();
            let mut descriptor = peer.session.descriptor();
            descriptor.blocked = true;
            if let Err(error) = peer.session.update(descriptor) {
                self.app.flash(format!("{UNAVAILABLE}: {error}"));
            }
            let mut meta = self.app.state.session.meta.clone();
            meta.peer_controls = Some(peer.session.controls());
            AppSession::checkpoint(
                &mut self.app.state.session,
                None,
                meta,
                self.app.state.token_usage,
            );
            self.app.checkpoint_now();
        }
        self.peer.take();
        self.app.peer_manager.close();
    }

    fn sync_peer(&mut self, transition: bool) -> Dirty {
        let Some(peer) = &self.peer else {
            return Dirty::NO;
        };
        let previous = peer.session.descriptor();
        let mut descriptor = self.peer_descriptor(previous.inbound.clone());
        descriptor.blocked |= transition;
        if previous.session_id != descriptor.session_id || previous.cwd != descriptor.cwd {
            self.close_peer();
            return Dirty::NO;
        }
        let controls_changed = !same_controls(&previous, &descriptor);
        let changed = controls_changed
            || previous.name != descriptor.name
            || previous.busy != descriptor.busy;
        let Some(peer) = &mut self.peer else {
            return Dirty::NO;
        };
        if controls_changed {
            peer.reviewed.clear();
        }
        if changed && let Err(error) = peer.session.update(descriptor) {
            self.close_peer();
            self.app.flash(format!("{UNAVAILABLE}: {error}"));
            return Dirty::YES;
        }
        let dirty = peer.sync_manager(&mut self.app);
        let held_count = peer.session.held_count();
        if held_count == peer.held_count {
            return dirty;
        }
        let arrivals = held_count > peer.held_count;
        peer.held_count = held_count;
        if arrivals && !self.app.peer_manager.is_open() {
            self.app.main_chat().push(DisplayMessage::new(
                DisplayRole::Notice,
                format!(
                    "{held_count} peer message(s) held; /messages to inspect and approve or reject"
                ),
            ));
        }
        Dirty::YES
    }

    fn peer_wake_ready(&self) -> bool {
        !self.peer_blocked()
            && !self.app.has_modal_overlay()
            && self.quiescent()
            && self
                .peer
                .as_ref()
                .is_some_and(|peer| peer.session.has_pending())
    }

    fn peer_notice(&mut self, text: String) {
        self.app
            .main_chat()
            .push(DisplayMessage::new(DisplayRole::Notice, text));
    }
}

impl EventLoop<'_> {
    pub(super) fn sync_peers(&mut self) -> Dirty {
        if self.ctx.peer_host.is_none() {
            return Dirty::NO;
        }
        let transition = self.relocation.is_some()
            || self.sandbox.is_some()
            || self.sandbox_control.is_some()
            || !self.sandbox_workflows.is_empty()
            || self
                .sessions
                .iter()
                .any(|runtime| !runtime.restore_transitions.is_empty());
        let mut dirty = Dirty::NO;
        for runtime in &mut self.sessions {
            dirty |= runtime.sync_peer(transition);
        }
        dirty
    }

    pub(super) fn start_peer_runs(&mut self) -> Dirty {
        if self.ctx.peer_host.is_none()
            || self.relocation.is_some()
            || self.sandbox.is_some()
            || self.sandbox_control.is_some()
            || !self.sandbox_workflows.is_empty()
            || self
                .sessions
                .iter()
                .any(|runtime| !runtime.restore_transitions.is_empty())
            || crate::sandbox::transfer::active()
        {
            return Dirty::NO;
        }
        let mut dirty = Dirty::NO;
        for index in 0..self.sessions.len() {
            if !self.sessions[index].peer_wake_ready() {
                continue;
            }
            // Admission owns the claim; a failed start must leave the inbox untouched.
            let actions = self.sessions[index].app.start_mailbox_run(Vec::new());
            if actions.is_empty() {
                self.sessions[index].app.suppress_background_wakes();
            }
            self.dispatch(index, actions);
            dirty = Dirty::YES;
        }
        dirty
    }

    fn peer_command_ready(&mut self, index: usize) -> bool {
        if self.sessions[index]
            .app
            .refuse_disabled(Feature::CrossSessionMessaging)
        {
            return false;
        }
        let _ = self.sync_peers();
        if self.sessions[index].peer.is_none() {
            self.sessions[index].app.flash(UNAVAILABLE.into());
            return false;
        }
        true
    }

    pub(super) fn list_peers(&mut self, index: usize) {
        self.open_peers(index, PeerView::Sessions);
    }

    fn open_peers(&mut self, index: usize, view: PeerView) {
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        if runtime.app.awaiting_input()
            || runtime.app.lifecycle_blocker().is_some()
            || runtime.app.holds_recovery_text()
        {
            runtime.app.flash(MODAL_BLOCKED.into());
            return;
        }
        let discover = view == PeerView::Sessions;
        runtime.app.open_peer_manager(view);
        if let Some(peer) = &mut runtime.peer {
            let _ = peer.sync_manager(&mut runtime.app);
        }
        if discover {
            self.refresh_peers(index);
        }
    }

    pub(super) fn refresh_peers(&mut self, index: usize) {
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        if !runtime.app.peer_manager.is_open() {
            return;
        }
        if let Some(peer) = &mut runtime.peer
            && peer.discover(runtime.app.peer_manager.generation())
        {
            runtime.app.peer_manager.start_discovery();
        }
    }

    pub(super) fn review_peer_message(&mut self, index: usize, id: &str) {
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        if !runtime.app.peer_manager.is_open() {
            return;
        }
        if let Some(peer) = &runtime.peer {
            runtime
                .app
                .peer_manager
                .set_review(peer.session.review_held(id));
        }
    }

    pub(super) fn decide_peer_message(
        &mut self,
        index: usize,
        token: PeerReviewToken,
        decision: PeerDecision,
    ) {
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        if let Some(peer) = &mut runtime.peer {
            peer.reviewed.retain(|_, reviewed| reviewed != &token);
            let result = peer.session.decide_held(&token, decision);
            if runtime.app.peer_manager.is_open() {
                runtime.app.peer_manager.finish_decision(result);
                let _ = peer.sync_manager(&mut runtime.app);
            } else {
                runtime.app.flash(match result {
                    Ok(result) => decision_notice(&result).into(),
                    Err(error) => error,
                });
            }
        }
    }

    pub(super) fn set_peer_inbound(&mut self, index: usize, policy: InboundPolicy) {
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        if let Some(peer) = &mut runtime.peer {
            let result = peer.session.set_inbound(policy.clone());
            if result.is_ok() {
                peer.reviewed.clear();
            }
            if runtime.app.peer_manager.is_open() {
                runtime.app.peer_manager.finish_policy(result);
                let _ = peer.sync_manager(&mut runtime.app);
            } else {
                runtime.app.flash(match result {
                    Ok(()) => format!("Inbound peer policy for this session: {policy:?}"),
                    Err(error) => error,
                });
            }
        }
    }

    pub(super) fn peer_messages(&mut self, index: usize, args: &str) {
        if args.trim() == "help" {
            let runtime = &mut self.sessions[index];
            if !runtime.app.refuse_disabled(Feature::CrossSessionMessaging) {
                runtime.peer_notice(MESSAGES_HELP.into());
            }
            return;
        }
        if !self.peer_command_ready(index) {
            return;
        }
        let args: Vec<_> = args.split_whitespace().collect();
        match args.as_slice() {
            [] => self.open_peers(index, PeerView::Held),
            [action @ ("approve" | "reject"), id] => {
                let Some(token) = self.sessions[index]
                    .peer
                    .as_mut()
                    .and_then(|peer| peer.reviewed.remove(*id))
                else {
                    self.sessions[index].app.flash(REVIEW_REQUIRED.into());
                    return;
                };
                let decision = if *action == "approve" {
                    PeerDecision::Approve
                } else {
                    PeerDecision::Reject
                };
                self.decide_peer_message(index, token, decision);
            }
            ["inbound", policy] => {
                let policy = match *policy {
                    "auto" => InboundPolicy::Auto,
                    "accept" => InboundPolicy::Accept,
                    "hold" => InboundPolicy::Hold,
                    "refuse" => InboundPolicy::Refuse,
                    _ => {
                        self.sessions[index].app.flash(MESSAGES_HELP.into());
                        return;
                    }
                };
                self.set_peer_inbound(index, policy);
            }
            _ => self.sessions[index].app.flash(MESSAGES_HELP.into()),
        }
    }

    pub(super) fn peer_topics(&mut self, index: usize, args: &str) {
        if args.trim() == "help" {
            let runtime = &mut self.sessions[index];
            if !runtime.app.refuse_disabled(Feature::CrossSessionMessaging) {
                runtime.peer_notice(TOPICS_HELP.into());
            }
            return;
        }
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        let Some(session) = runtime.peer.as_ref().map(|peer| peer.session.clone()) else {
            return;
        };
        let current = session.controls();
        let args: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
        let result = match subscription_request(&current.topics, current.broadcasts, &args) {
            SubscriptionRequest::Show => {
                Ok(subscriptions_summary(&current.topics, current.broadcasts))
            }
            SubscriptionRequest::Usage => Err(TOPICS_HELP.to_owned()),
            SubscriptionRequest::Change(change) => change.and_then(|(topics, broadcasts)| {
                let summary = subscriptions_summary(&topics, broadcasts);
                session
                    .set_subscriptions(topics, broadcasts)
                    .map(|()| summary)
            }),
        };
        match result {
            Ok(summary) => runtime.peer_notice(summary),
            Err(error) => runtime.app.flash(error),
        }
    }
}

#[derive(Debug, PartialEq)]
enum SubscriptionRequest {
    Show,
    Usage,
    /// The whole subscription set the command asks for, topics then broadcasts.
    Change(Result<(Vec<String>, bool), String>),
}

fn subscription_request(
    topics: &[String],
    broadcasts: bool,
    args: &[String],
) -> SubscriptionRequest {
    match args.split_first() {
        None => SubscriptionRequest::Show,
        Some((action, patterns)) if action == "subscribe" && !patterns.is_empty() => {
            SubscriptionRequest::Change(
                add_patterns(topics, patterns).map(|topics| (topics, broadcasts)),
            )
        }
        Some((action, patterns)) if action == "unsubscribe" && !patterns.is_empty() => {
            SubscriptionRequest::Change(
                remove_patterns(topics, patterns).map(|topics| (topics, broadcasts)),
            )
        }
        Some((action, [state])) if action == "broadcast" && (state == "on" || state == "off") => {
            SubscriptionRequest::Change(Ok((topics.to_vec(), state == "on")))
        }
        Some(_) => SubscriptionRequest::Usage,
    }
}

fn subscriptions_summary(topics: &[String], broadcasts: bool) -> String {
    let topics = if topics.is_empty() {
        NO_TOPICS.to_owned()
    } else {
        format!("{TOPICS_LABEL}{}", topics.join(", "))
    };
    let broadcasts = if broadcasts {
        BROADCASTS_ON
    } else {
        BROADCASTS_OFF
    };
    format!("{topics}{SUMMARY_SEPARATOR}{broadcasts}")
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use caudra_agent::peers::topics::{INVALID_PATTERN, MISSING_PATTERN};
    use caudra_config::{Feature, FeatureFlags, sandbox::SandboxName};
    use flume::Sender;
    use test_case::test_case;

    use super::{
        BROADCASTS_OFF, BROADCASTS_ON, DISCOVERY_STOPPED, DiscoveryResult, NO_TOPICS,
        PeerDiscovery, SubscriptionRequest, TOPICS_LABEL, peer_blocked, peer_eligible,
        poll_discovery, subscription_request, subscriptions_summary,
    };
    use crate::app::{App, tests::test_app};
    use crate::components::{ExitRequest, Status};

    const ERROR: &str = "The model request failed";
    const GENERATION: u64 = 7;
    const SUBSCRIBED: &str = "ci.*";
    const ADDED: &str = "deploy.**";

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    #[test_case(&[], SubscriptionRequest::Show; "no_arguments_show")]
    #[test_case(&["subscribe", ADDED, SUBSCRIBED], SubscriptionRequest::Change(Ok((words(&[SUBSCRIBED, ADDED]), false))); "subscribe_adds_new_patterns")]
    #[test_case(&["unsubscribe", SUBSCRIBED], SubscriptionRequest::Change(Ok((Vec::new(), false))); "unsubscribe_removes")]
    #[test_case(&["unsubscribe", ADDED], SubscriptionRequest::Change(Err(format!("{MISSING_PATTERN}: {ADDED:?}"))); "unsubscribe_names_a_missing_pattern")]
    #[test_case(&["subscribe", "CI"], SubscriptionRequest::Change(Err(INVALID_PATTERN.into())); "subscribe_validates")]
    #[test_case(&["broadcast", "on"], SubscriptionRequest::Change(Ok((words(&[SUBSCRIBED]), true))); "broadcast_on")]
    #[test_case(&["broadcast", "maybe"], SubscriptionRequest::Usage; "broadcast_needs_on_or_off")]
    #[test_case(&["subscribe"], SubscriptionRequest::Usage; "subscribe_needs_a_pattern")]
    #[test_case(&["list"], SubscriptionRequest::Usage; "unknown_action")]
    fn topics_commands_describe_the_whole_subscription_set(
        args: &[&str],
        expected: SubscriptionRequest,
    ) {
        assert_eq!(
            subscription_request(&words(&[SUBSCRIBED]), false, &words(args)),
            expected
        );
    }

    #[test_case(&[], false, &format!("{NO_TOPICS} · {BROADCASTS_OFF}"); "nothing")]
    #[test_case(&[SUBSCRIBED, ADDED], true, &format!("{TOPICS_LABEL}{SUBSCRIBED}, {ADDED} · {BROADCASTS_ON}"); "topics_and_broadcasts")]
    fn subscription_summaries_name_topics_and_broadcasts(
        topics: &[&str],
        broadcasts: bool,
        expected: &str,
    ) {
        assert_eq!(subscriptions_summary(&words(topics), broadcasts), expected);
    }

    fn discovery() -> (Sender<DiscoveryResult>, Option<PeerDiscovery>) {
        let (sender, receiver) = flume::bounded(1);
        (
            sender,
            Some(PeerDiscovery {
                generation: GENERATION,
                receiver,
                _task: smol::spawn(pending()),
            }),
        )
    }

    #[test_case(false, false, false; "disabled_local")]
    #[test_case(false, true, false; "disabled_sandbox")]
    #[test_case(true, false, true; "enabled_local")]
    #[test_case(true, true, false; "enabled_sandbox")]
    fn registration_requires_an_enabled_local_runtime(
        enabled: bool,
        sandbox: bool,
        expected: bool,
    ) {
        let mut app = test_app();
        app.features = if enabled {
            FeatureFlags::NONE.with(Feature::CrossSessionMessaging)
        } else {
            FeatureFlags::NONE
        };
        app.sandbox_live.name = sandbox.then(|| SandboxName::parse("test").unwrap());
        assert_eq!(peer_eligible(&app), expected);
    }

    #[test_case(|app| app.automatic_wakes_suppressed = true; "cancelled")]
    #[test_case(|app| app.cancelling_run = Some(1); "cancellation_in_flight")]
    #[test_case(|app| app.exit_request = ExitRequest::Reload; "reload")]
    #[test_case(|app| app.status = Status::error(ERROR.into()); "request_failure")]
    fn unsafe_runtime_blocks_peer_admission(block: fn(&mut App)) {
        let mut app = test_app();
        assert!(!peer_blocked(&app));
        block(&mut app);
        assert!(peer_blocked(&app));
    }

    #[test_case(Some(GENERATION), true; "current_opening")]
    #[test_case(Some(GENERATION + 1), false; "reopened_modal")]
    #[test_case(None, false; "closed_modal")]
    fn discovery_results_belong_to_one_opening(generation: Option<u64>, accepted: bool) {
        let (sender, mut discovery) = discovery();
        sender.send(Ok(Vec::new())).unwrap();
        let result = poll_discovery(&mut discovery, generation);
        assert_eq!(result.is_some(), accepted);
        assert!(discovery.is_none());
        assert!(poll_discovery(&mut discovery, generation).is_none());
    }

    #[test_case(false; "pending")]
    #[test_case(true; "worker_disconnected")]
    fn pending_discovery_is_nonblocking_and_reports_worker_loss(disconnect: bool) {
        let (sender, mut discovery) = discovery();
        let sender = (!disconnect).then_some(sender);
        let result = poll_discovery(&mut discovery, Some(GENERATION));
        if disconnect {
            assert_eq!(result.unwrap().unwrap_err(), DISCOVERY_STOPPED);
            assert!(discovery.is_none());
        } else {
            assert!(result.is_none());
            assert!(discovery.is_some());
        }
        drop(sender);
    }
}
