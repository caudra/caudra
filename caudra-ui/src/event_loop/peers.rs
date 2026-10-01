use std::collections::HashMap;

use caudra_agent::peers::{HeldMessage, PeerDescriptor, PeerSession};
use caudra_config::{Feature, InboundPolicy};

use super::{EventLoop, SessionRuntime, SessionStatus, SpawnCtx};
use crate::AppSession;
use crate::app::App;
use crate::components::{DisplayMessage, DisplayRole, ExitRequest, Status};
use crate::repaint::Dirty;

const UNAVAILABLE: &str = "Cross-session messaging is unavailable for this runtime";
const REVIEW_REQUIRED: &str =
    "Message review expired or was not opened; run /messages and review the current message ID";
const MESSAGES_HELP: &str = "Usage: /messages [help | approve ID | reject ID | inbound auto|accept|hold|refuse]\nRun /messages to inspect held content before approving or rejecting its ID. Approval accepts that message, not its requested tool actions. Accept can start billable model turns using this session's existing permissions. Hold requires review; refuse rejects arrivals. Project refusal cannot be overridden. Pending messages are live-only and expire when the runtime closes.";

pub(super) struct PeerRegistration {
    session: PeerSession,
    reviewed: HashMap<String, u64>,
    held_count: usize,
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

fn take_review(reviewed: &mut HashMap<String, u64>, held: &[HeldMessage], id: &str) -> bool {
    reviewed.remove(id).is_some_and(|epoch| {
        held.iter()
            .any(|message| message.message_id == id && message.epoch == epoch)
    })
}

impl SessionRuntime {
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
            ctx.config.messaging.project_inbound.clone(),
            self.app.state.session.meta.peer_controls.clone(),
        ) {
            Ok(session) => {
                self.peer = Some(PeerRegistration {
                    session,
                    reviewed: HashMap::new(),
                    held_count: 0,
                });
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
        let held_count = peer.session.held_count();
        if held_count == peer.held_count {
            return Dirty::NO;
        }
        peer.held_count = held_count;
        if held_count > 0 {
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
        if !self.peer_command_ready(index) {
            return;
        }
        let runtime = &mut self.sessions[index];
        let Some(peer) = &runtime.peer else { return };
        match smol::block_on(peer.session.list()) {
            Ok(peers) => {
                let mut text = String::from("Live local peers (run /peers to refresh):\n");
                if peers.is_empty() {
                    text.push_str(
                        "No reachable peers. Both processes must enable cross-session messaging.",
                    );
                }
                for peer in peers {
                    let state = if peer.blocked {
                        "blocked"
                    } else if peer.busy {
                        "busy"
                    } else {
                        "idle"
                    };
                    text.push_str(&format!(
                        "\n{:?} · {state} · inbound {:?}\nSession: {}\nWorkspace: {:?}\nTarget: {:?}\n",
                        peer.name, peer.inbound, peer.session_id, peer.cwd, peer.target,
                    ));
                }
                runtime.peer_notice(text);
            }
            Err(error) => runtime.app.flash(format!("Peer discovery failed: {error}")),
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
        let runtime = &mut self.sessions[index];
        let Some(peer) = &mut runtime.peer else {
            return;
        };
        let args: Vec<_> = args.split_whitespace().collect();
        match args.as_slice() {
            [] => {
                let held = peer.session.held();
                peer.reviewed = held
                    .iter()
                    .map(|message| (message.message_id.clone(), message.epoch))
                    .collect();
                let mut text = format!(
                    "Peer messages · inbound {:?}\n{MESSAGES_HELP}\n",
                    peer.session.descriptor().inbound
                );
                if held.is_empty() {
                    text.push_str("\nNo held messages.");
                }
                for message in held {
                    text.push_str(&format!(
                        "\nID: {:?}\nFrom: {:?}\nHeld: {:?}\nText: {:?}\n",
                        message.message_id, message.sender_name, message.reason, message.text
                    ));
                }
                runtime.peer_notice(text);
            }
            [action @ ("approve" | "reject"), id] => {
                if !take_review(&mut peer.reviewed, &peer.session.held(), id) {
                    runtime.app.flash(REVIEW_REQUIRED.into());
                    return;
                }
                let result = if *action == "approve" {
                    peer.session.approve(id)
                } else {
                    peer.session.reject(id)
                };
                match result {
                    Ok(()) => runtime
                        .app
                        .flash(format!("Peer message {id}: {action} accepted")),
                    Err(error) => runtime.app.flash(error),
                }
            }
            ["inbound", policy] => {
                let policy = match *policy {
                    "auto" => InboundPolicy::Auto,
                    "accept" => InboundPolicy::Accept,
                    "hold" => InboundPolicy::Hold,
                    "refuse" => InboundPolicy::Refuse,
                    _ => {
                        runtime.app.flash(MESSAGES_HELP.into());
                        return;
                    }
                };
                match peer.session.set_inbound(policy.clone()) {
                    Ok(()) => {
                        peer.reviewed.clear();
                        runtime.peer_notice(format!(
                            "Inbound peer policy: {policy:?}\n{MESSAGES_HELP}"
                        ));
                    }
                    Err(error) => runtime.app.flash(error),
                }
            }
            _ => runtime.app.flash(MESSAGES_HELP.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use caudra_agent::peers::HeldMessage;
    use caudra_config::{Feature, FeatureFlags, sandbox::SandboxName};
    use test_case::test_case;

    use super::{peer_blocked, peer_eligible, take_review};
    use crate::app::{App, tests::test_app};
    use crate::components::{ExitRequest, Status};

    const MESSAGE_ID: &str = "reviewed-message";
    const OTHER_ID: &str = "another-message";
    const SENDER: &str = "reviewer";
    const BODY: &str = "The validation failed.";
    const HELD: &str = "Explicit hold policy";
    const ERROR: &str = "The model request failed";
    const REVIEW_EPOCH: u64 = 7;

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

    #[test_case(|app| app.automatic_wakes_suppressed = true; "cancelled_or_exhausted")]
    #[test_case(|app| app.cancelling_run = Some(1); "cancellation_in_flight")]
    #[test_case(|app| app.exit_request = ExitRequest::Reload; "reload")]
    #[test_case(|app| app.status = Status::error(ERROR.into()); "request_failure")]
    fn unsafe_runtime_blocks_peer_admission(block: fn(&mut App)) {
        let mut app = test_app();
        assert!(!peer_blocked(&app));
        block(&mut app);
        assert!(peer_blocked(&app));
    }

    #[test_case(MESSAGE_ID, REVIEW_EPOCH, true; "current_review")]
    #[test_case(MESSAGE_ID, REVIEW_EPOCH + 1, false; "control_epoch_changed")]
    #[test_case(OTHER_ID, REVIEW_EPOCH, false; "different_message")]
    fn held_decision_is_bound_to_inspected_id_and_epoch(id: &str, epoch: u64, expected: bool) {
        let mut reviewed = HashMap::from([(MESSAGE_ID.into(), REVIEW_EPOCH)]);
        let held = vec![HeldMessage {
            message_id: id.into(),
            sender_name: SENDER.into(),
            text: BODY.into(),
            reason: HELD.into(),
            epoch,
        }];
        assert_eq!(take_review(&mut reviewed, &held, MESSAGE_ID), expected);
        assert!(!take_review(&mut reviewed, &held, MESSAGE_ID));
    }

    #[test_case(true; "registration_replaced")]
    #[test_case(false; "message_no_longer_held")]
    fn stale_held_review_cannot_approve_or_reject(replaced: bool) {
        let mut reviewed = if replaced {
            HashMap::new()
        } else {
            HashMap::from([(MESSAGE_ID.into(), REVIEW_EPOCH)])
        };
        assert!(!take_review(&mut reviewed, &[], MESSAGE_ID));
    }
}
