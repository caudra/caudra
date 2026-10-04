use std::collections::HashMap;
use std::future::Future;
use std::time::Instant;

use caudra_agent::peers::{
    ChannelPage, ChannelSummary, HistoryVersion, MAX_HISTORY_PAGE, PeerDecision,
    PeerDecisionResult, PeerDescriptor, PeerReviewToken, PeerSession, PeerSummary,
};
use caudra_config::{Feature, InboundPolicy};
use flume::{Receiver, TryRecvError};
use smol::Task;
use tracing::warn;

use super::{EventLoop, SessionRuntime, SessionStatus, SpawnCtx};
use crate::AppSession;
use crate::app::App;
use crate::components::peer_manager::{
    HISTORY_POLL, PageRequest, PeerManager, PeerView, SubscriptionChange,
};
use crate::components::{DisplayMessage, DisplayRole, ExitRequest, Status};
use crate::repaint::Dirty;

const UNAVAILABLE: &str = "Cross-session messaging is unavailable for this runtime";
const REVIEW_REQUIRED: &str =
    "Message review expired or was not opened; run /messages and open that message's review";
const MESSAGES_HELP: &str = "Usage: /messages [help | approve ID | reject ID | inbound auto|accept|hold|refuse]\nRun /messages to open the Held messages view. Enter opens one message's review before approval or rejection. Approval accepts that message, not its requested tool actions, and can start a billable model turn. Inbound policy applies to this session and cannot relax project restrictions. Pending messages are live-only and expire when the runtime closes.";
const LOAD_STOPPED: &str = "Peer messaging stopped before returning a result";
const MODAL_BLOCKED: &str = "Finish the pending session review before opening the peer manager";
const QUEUED: &str = "Message queued for the next safe boundary; an idle session may start after closing the manager";
const REJECTED: &str = "Message rejected and removed from the live inbox";
const TOPICS_HELP: &str = "Usage: /topics [help | subscribe PATTERN... | unsubscribe PATTERN... | broadcast on|off]\nRun /topics to browse stored topic messages in the peer manager, where s subscribes to the selected topic and p edits this session's subscriptions. A topic is a dot-separated name such as ci.failures. In a pattern, * matches one segment and a final ** matches one or more. Subscriptions decide which publications reach this session, up to 16 patterns. Broadcasts reach only sessions that turn them on. The inbound policy still decides whether each message is delivered, held, or refused. Subscriptions are kept with the session and restored when it resumes.";
const TOPICS_LABEL: &str = "Peer topics: ";
const NO_TOPICS: &str = "No peer topic subscriptions";
const BROADCASTS_ON: &str = "broadcasts on";
const BROADCASTS_OFF: &str = "broadcasts off";
const SUMMARY_SEPARATOR: &str = " · ";

pub(super) struct PeerRegistration {
    session: PeerSession,
    reviewed: HashMap<String, PeerReviewToken>,
    held_count: usize,
    discovery: Option<Load<u64, Vec<PeerSummary>>>,
    history: HistoryLoads,
    catch_up: Option<Task<()>>,
}

/// One answer a worker owes the open manager, keyed by the opening and the
/// question asked, so an answer to anything else is never taken.
struct Load<K, T> {
    key: K,
    receiver: Receiver<Result<T, String>>,
    _task: Task<()>,
}

impl<K, T: Send + 'static> Load<K, T> {
    fn start(key: K, work: impl Future<Output = Result<T, String>> + Send + 'static) -> Self {
        let (sender, receiver) = flume::bounded(1);
        Self {
            key,
            receiver,
            _task: smol::spawn(async move {
                let _ = sender.try_send(work.await);
            }),
        }
    }
}

/// What the Messages view waits on: a version poll, a channel list, and a
/// page, at most one of each. A history change does not restart a load; the
/// view asks again once it lands.
#[derive(Default)]
struct HistoryLoads {
    version: Option<Load<u64, HistoryVersion>>,
    polled: Option<(u64, Instant)>,
    channels: Option<Load<u64, Vec<ChannelSummary>>>,
    page: Option<Load<(u64, PageRequest), ChannelPage>>,
}

impl PeerRegistration {
    fn new(session: PeerSession) -> Self {
        let mut registration = Self {
            session,
            reviewed: HashMap::new(),
            held_count: 0,
            discovery: None,
            history: HistoryLoads::default(),
            catch_up: None,
        };
        registration.catch_up();
        registration
    }

    /// Offers the newest unseen message on each subscribed topic; they ride
    /// along with the next turn without starting one.
    fn catch_up(&mut self) {
        let session = self.session.clone();
        self.catch_up = Some(smol::spawn(async move {
            if let Err(error) = session.catch_up().await {
                warn!(%error, "peer message catch-up failed");
            }
        }));
    }

    fn discover(&mut self, generation: u64) -> bool {
        if self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.key == generation)
        {
            return false;
        }
        let session = self.session.clone();
        self.discovery = Some(Load::start(generation, async move {
            session.list_named().await
        }));
        true
    }

    fn sync_manager(&mut self, app: &mut App) -> Dirty {
        let manager = &mut app.peer_manager;
        let generation = manager.is_open().then(|| manager.generation());
        let mut dirty = Dirty::NO;
        if let Some((_, result)) = poll_load(&mut self.discovery, generation.as_ref()) {
            manager.set_sessions(result);
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
                    dirty |= Dirty::from(manager.update_inbox(snapshot));
                }
                Err(error) => {
                    manager.set_error(error);
                    dirty = Dirty::YES;
                }
            }
            dirty |= Dirty::from(manager.update_controls(self.session.controls()));
            dirty |= Dirty::from(manager.update_messaging_name(self.session.handle()));
        }
        dirty |= self.sync_history(manager, generation);
        dirty
    }

    /// Hands the Messages view whatever its loads answered, then asks for
    /// what it still waits on. The version is polled every `HISTORY_POLL`.
    fn sync_history(&mut self, manager: &mut PeerManager, generation: Option<u64>) -> Dirty {
        let visible = generation.filter(|_| manager.history_visible());
        let loads = &mut self.history;
        let mut dirty = Dirty::NO;
        if let Some((_, result)) = poll_load(&mut loads.version, visible.as_ref()) {
            dirty |= Dirty::from(manager.set_history_version(result));
        }
        let channels = generation.filter(|_| manager.wanted_channels());
        if let Some((_, result)) = poll_load(&mut loads.channels, channels.as_ref()) {
            manager.set_channels(result);
            dirty = Dirty::YES;
        }
        let page = generation.zip(manager.wanted_page().cloned());
        if let Some(((_, request), result)) = poll_load(&mut loads.page, page.as_ref()) {
            manager.set_page(request, result);
            dirty = Dirty::YES;
        }
        let now = Instant::now();
        if let Some(generation) = visible
            && loads.version.is_none()
            && poll_due(loads.polled, generation, now)
        {
            loads.polled = Some((generation, now));
            let session = self.session.clone();
            loads.version = Some(Load::start(generation, async move {
                session.history_version().await
            }));
        }
        manager.set_history_polling(loads.version.is_some());
        ensure_load(
            &mut loads.channels,
            generation.filter(|_| manager.wanted_channels()),
            |_| {
                let session = self.session.clone();
                async move { session.message_channels().await }
            },
        );
        ensure_load(
            &mut loads.page,
            generation.zip(manager.wanted_page().cloned()),
            |(_, request)| {
                let session = self.session.clone();
                let (channel, before) = (request.channel.clone(), request.before);
                async move {
                    session
                        .channel_messages(channel, before, MAX_HISTORY_PAGE)
                        .await
                }
            },
        );
        dirty
    }
}

/// The answer to `wanted`, once it has come. A load that asked anything else
/// is dropped, which cancels it.
fn poll_load<K: PartialEq, T>(
    pending: &mut Option<Load<K, T>>,
    wanted: Option<&K>,
) -> Option<(K, Result<T, String>)> {
    let load = pending.as_ref()?;
    if wanted != Some(&load.key) {
        *pending = None;
        return None;
    }
    let result = match load.receiver.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Err(LOAD_STOPPED.into()),
    };
    pending.take().map(|load| (load.key, result))
}

/// Starts `work` for `wanted` unless a load already asks it, and drops a load
/// nothing wants.
fn ensure_load<K, T, F>(
    pending: &mut Option<Load<K, T>>,
    wanted: Option<K>,
    work: impl FnOnce(&K) -> F,
) where
    K: PartialEq,
    T: Send + 'static,
    F: Future<Output = Result<T, String>> + Send + 'static,
{
    match wanted {
        None => *pending = None,
        Some(key) if pending.as_ref().is_none_or(|load| load.key != key) => {
            let work = work(&key);
            *pending = Some(Load::start(key, work));
        }
        Some(_) => {}
    }
}

/// Whether the history version is due a poll: at once in a new opening of
/// the manager, then once every `HISTORY_POLL`.
fn poll_due(polled: Option<(u64, Instant)>, generation: u64, now: Instant) -> bool {
    polled.is_none_or(|(opening, at)| {
        opening != generation || now.saturating_duration_since(at) >= HISTORY_POLL
    })
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
                    self.peer_notice(error);
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
        let args: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
        match subscription_request(&args) {
            SubscriptionRequest::Browse => {
                self.open_peers(index, PeerView::Messages);
                self.sessions[index].app.peer_manager.show_only_topics();
            }
            SubscriptionRequest::Change(change) => self.change_peer_subscriptions(index, change),
            SubscriptionRequest::Usage => {
                if self.peer_command_ready(index) {
                    self.sessions[index].app.flash(TOPICS_HELP.to_owned());
                }
            }
        }
    }

    /// Applies `change` to the subscriptions this session holds now, then
    /// offers what the new set has missed. `/topics` and the peer manager both
    /// change subscriptions here.
    pub(super) fn change_peer_subscriptions(&mut self, index: usize, change: SubscriptionChange) {
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        let Some(peer) = &mut runtime.peer else {
            return;
        };
        let current = peer.session.controls();
        let result =
            change
                .apply(&current.topics, current.broadcasts)
                .and_then(|(topics, broadcasts)| {
                    let summary = subscriptions_summary(&topics, broadcasts);
                    peer.session
                        .set_subscriptions(topics, broadcasts)
                        .map(|()| summary)
                });
        if result.is_ok() {
            peer.catch_up();
        }
        if runtime.app.peer_manager.is_open() {
            runtime.app.peer_manager.finish_subscriptions(result);
            let _ = peer.sync_manager(&mut runtime.app);
            return;
        }
        match result {
            Ok(summary) => runtime.peer_notice(summary),
            Err(error) => runtime.app.flash(error),
        }
    }
}

#[derive(Debug, PartialEq)]
enum SubscriptionRequest {
    Browse,
    Usage,
    Change(SubscriptionChange),
}

fn subscription_request(args: &[String]) -> SubscriptionRequest {
    match args.split_first() {
        None => SubscriptionRequest::Browse,
        Some((action, patterns)) if action == "subscribe" && !patterns.is_empty() => {
            SubscriptionRequest::Change(SubscriptionChange::Subscribe(patterns.to_vec()))
        }
        Some((action, patterns)) if action == "unsubscribe" && !patterns.is_empty() => {
            SubscriptionRequest::Change(SubscriptionChange::Unsubscribe(patterns.to_vec()))
        }
        Some((action, [state])) if action == "broadcast" && (state == "on" || state == "off") => {
            SubscriptionRequest::Change(SubscriptionChange::Broadcasts(state == "on"))
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
    use std::time::{Duration, Instant};

    use caudra_config::{Feature, FeatureFlags, sandbox::SandboxName};
    use flume::Sender;
    use test_case::test_case;

    use super::{
        BROADCASTS_OFF, BROADCASTS_ON, LOAD_STOPPED, Load, NO_TOPICS, SubscriptionRequest,
        TOPICS_LABEL, ensure_load, peer_blocked, peer_eligible, poll_due, poll_load,
        subscription_request, subscriptions_summary,
    };
    use crate::app::{App, tests::test_app};
    use crate::components::peer_manager::{HISTORY_POLL, SubscriptionChange};
    use crate::components::{ExitRequest, Status};

    const ERROR: &str = "The model request failed";
    const GENERATION: u64 = 7;
    const SUBSCRIBED: &str = "ci.*";
    const ADDED: &str = "deploy.**";

    type Answer = Result<(), String>;

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    #[test_case(&[], SubscriptionRequest::Browse; "no_arguments_browse")]
    #[test_case(&["subscribe", ADDED, SUBSCRIBED], SubscriptionRequest::Change(SubscriptionChange::Subscribe(words(&[ADDED, SUBSCRIBED]))); "subscribe_names_patterns")]
    #[test_case(&["unsubscribe", SUBSCRIBED], SubscriptionRequest::Change(SubscriptionChange::Unsubscribe(words(&[SUBSCRIBED]))); "unsubscribe_names_patterns")]
    #[test_case(&["broadcast", "on"], SubscriptionRequest::Change(SubscriptionChange::Broadcasts(true)); "broadcast_on")]
    #[test_case(&["broadcast", "off"], SubscriptionRequest::Change(SubscriptionChange::Broadcasts(false)); "broadcast_off")]
    #[test_case(&["broadcast", "maybe"], SubscriptionRequest::Usage; "broadcast_needs_on_or_off")]
    #[test_case(&["subscribe"], SubscriptionRequest::Usage; "subscribe_needs_a_pattern")]
    #[test_case(&["list"], SubscriptionRequest::Usage; "unknown_action")]
    fn topics_commands_name_one_subscription_change(args: &[&str], expected: SubscriptionRequest) {
        assert_eq!(subscription_request(&words(args)), expected);
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

    fn load() -> (Sender<Answer>, Option<Load<u64, ()>>) {
        let (sender, receiver) = flume::bounded(1);
        (
            sender,
            Some(Load {
                key: GENERATION,
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

    #[test_case(Some(GENERATION), true; "current_question")]
    #[test_case(Some(GENERATION + 1), false; "another_question")]
    #[test_case(None, false; "nothing_wanted")]
    fn load_results_answer_only_the_question_asked(wanted: Option<u64>, accepted: bool) {
        let (sender, mut load) = load();
        sender.send(Ok(())).unwrap();
        let result = poll_load(&mut load, wanted.as_ref());
        assert_eq!(result.is_some(), accepted);
        assert!(load.is_none());
        assert!(poll_load(&mut load, wanted.as_ref()).is_none());
    }

    #[test_case(false; "pending")]
    #[test_case(true; "worker_disconnected")]
    fn pending_loads_are_nonblocking_and_report_worker_loss(disconnect: bool) {
        let (sender, mut load) = load();
        let sender = (!disconnect).then_some(sender);
        let result = poll_load(&mut load, Some(&GENERATION));
        if disconnect {
            assert_eq!(result.unwrap().1.unwrap_err(), LOAD_STOPPED);
            assert!(load.is_none());
        } else {
            assert!(result.is_none());
            assert!(load.is_some());
        }
        drop(sender);
    }

    #[test]
    fn loads_start_once_per_question_and_stop_when_unwanted() {
        let mut load: Option<Load<u64, ()>> = None;
        let mut started = Vec::new();
        for wanted in [
            Some(GENERATION),
            Some(GENERATION),
            Some(GENERATION + 1),
            None,
        ] {
            ensure_load(&mut load, wanted, |key| {
                started.push(*key);
                pending()
            });
            assert_eq!(load.as_ref().map(|load| load.key), wanted);
        }
        assert_eq!(started, [GENERATION, GENERATION + 1]);
    }

    #[test_case(None, Duration::ZERO, true; "first_poll")]
    #[test_case(Some(GENERATION), Duration::ZERO, false; "just_polled")]
    #[test_case(Some(GENERATION), HISTORY_POLL, true; "interval_elapsed")]
    #[test_case(Some(GENERATION - 1), Duration::ZERO, true; "reopened_manager")]
    fn history_version_polls_at_once_per_opening_then_every_interval(
        opening: Option<u64>,
        elapsed: Duration,
        due: bool,
    ) {
        let polled_at = Instant::now();
        let polled = opening.map(|opening| (opening, polled_at));
        assert_eq!(poll_due(polled, GENERATION, polled_at + elapsed), due);
    }
}
