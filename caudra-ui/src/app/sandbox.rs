use super::App;
use crate::AppSession;
use crate::components::sandbox_manager::{SandboxAction, SandboxView};
use crate::components::{DisplayMessage, DisplayRole, Overlay};
use crate::repaint::Dirty;
use crate::sandbox::{LiveRequest, start_live, start_snapshot};
use crate::sandbox::{
    NETWORK_PENDING, NETWORK_RECOVERY, NETWORK_SAVE_NOTICE, NETWORK_SAVE_UNKNOWN,
    NetworkReconcileReport, NetworkReconcileRequest, StoreEffect, StoreResult,
    start_network_reconcile,
};
use crate::sandbox::{SandboxSnapshot, SandboxSnapshotRequest, SandboxWorkers, start_store_effect};
use caudra_config::sandbox::SandboxName;
use caudra_config::{Feature, FeatureDisabled};
use flume::TryRecvError;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const UNSAVED_DRAFT_ERR: &str =
    "Save or discard unsaved local editor/composer drafts before changing sandbox authority";
pub(super) const WORKBENCH_BUSY: &str =
    "Wait for pending Workbench reads or writes before changing sandbox authority";

/// The instance the footer names, or `None` when there is nothing to name.
/// Readiness is the manager's own: the binding's sandbox record says which
/// instance, and this runtime's authenticated connection says the session is
/// attached to it rather than the instance merely running. Takes its two halves
/// apart so a caller already holding the bar can still ask.
///
/// The name is the runtime's own, captured from the selection that built it,
/// because the binding a session stores keeps only an opaque record id and
/// resolving one costs a read of the saved records. A runtime handed no name
/// falls back to that id: it identifies the same instance less legibly, while
/// saying nothing would claim a detachment that is not there.
pub(crate) fn attached_sandbox_instance<'a>(
    session: &'a AppSession,
    live: &'a SandboxWorkers,
) -> Option<&'a str> {
    let binding = session.workspace_binding()?;
    binding.sandbox_record()?;
    live.readiness
        .as_ref()
        .is_some_and(|ready| ready())
        .then(|| {
            live.name
                .as_ref()
                .map_or_else(|| binding.server_id(), SandboxName::as_str)
        })
}

impl App {
    pub(super) fn open_sandbox(&mut self, args: &str) {
        if self.refuse_disabled(Feature::Sandboxes) {
            return;
        }
        if args.trim() == "reconcile-network" {
            if self
                .sandbox_live
                .network_gate
                .lock()
                .is_ok_and(|gate| gate.pending > 0)
            {
                self.flash(NETWORK_PENDING.into());
                return;
            }
            if let Some(saved) = self.sandbox_manager.baseline() {
                self.enqueue_network_reconcile(NetworkReconcileRequest {
                    saved,
                    networks: Default::default(),
                    recovery: true,
                });
                self.flash("Inspecting durable outcomes before reconciling saved networks. No unknown request is replayed. Reload /sandbox first if the saved revision changed.".into());
            } else {
                self.flash("Open /sandbox to load the committed configuration, then /sandbox reconcile-network.".into());
            }
            return;
        }
        let view = match args.trim() {
            "" | "status" | "instances" => SandboxView::Instances,
            "profiles" => SandboxView::Profiles,
            "images" => SandboxView::Images,
            "providers" | "doctor" => SandboxView::Providers,
            _ => {
                self.flash("Usage: /sandbox [instances|profiles|images|providers|status]".into());
                return;
            }
        };
        let action = self.sandbox_manager.open(self.state.session.id, view);
        self.handle_sandbox_action(action);
        let report = self
            .sandbox_live
            .network_gate
            .lock()
            .ok()
            .and_then(|gate| gate.latest_report.clone());
        if let Some(report) = report {
            self.sandbox_manager
                .receive_network_report(report.summary());
        }
    }

    pub fn sandbox_snapshot_request(&self) -> Option<SandboxSnapshotRequest> {
        self.sandbox_manager.snapshot_request(self.state.session.id)
    }

    /// Returns false for a different conversation, manager session, saved revision,
    /// provider revision, or an older/equal snapshot sequence. Never changes a draft.
    pub fn install_sandbox_snapshot(
        &mut self,
        request: &SandboxSnapshotRequest,
        snapshot: SandboxSnapshot,
    ) -> bool {
        if request.conversation != self.state.session.id {
            return false;
        }
        self.sandbox_manager.install_snapshot(request, snapshot)
    }

    pub(super) fn handle_sandbox_action(&mut self, action: SandboxAction) {
        if self.refuse_disabled(Feature::Sandboxes) {
            return;
        }
        match action {
            SandboxAction::None => {}
            SandboxAction::Copy(text) => self.copy_to_clipboard(&text),
            SandboxAction::Store { ticket, effect } => {
                self.sandbox_live.network_save = match &effect {
                    StoreEffect::Save { baseline, draft }
                        if baseline.saved().configuration().networks != draft.networks =>
                    {
                        if let Ok(mut gate) = self.sandbox_live.network_gate.lock() {
                            gate.pending += 1;
                        }
                        Some((ticket.clone(), baseline.clone()))
                    }
                    _ => None,
                };
                self.sandbox_reply = Some(start_store_effect(ticket, effect));
            }
            SandboxAction::Live(request) => self.sandbox_live.queued = Some(request),
        }
    }

    pub(crate) fn start_sandbox_live(&mut self, request: LiveRequest) {
        self.sandbox_live.reply = Some(start_live(
            request,
            self.storage.clone(),
            self.sandbox_live.connector.clone(),
        ));
    }

    pub(crate) fn sandbox_failed(&mut self, message: String) {
        self.sandbox_manager.live_failed(message);
    }

    /// Drops queued sandbox work in a process that left the experiment off,
    /// so nothing queued ahead of a check reaches a controller or a worker.
    pub(crate) fn refuse_sandbox_work(&mut self, disabled: &FeatureDisabled) -> Dirty {
        let live = self.sandbox_live.queued.take().is_some();
        let transfer = self.sandbox_live.transfer_queued.take().is_some();
        if live {
            self.sandbox_failed(disabled.to_string());
        }
        if transfer {
            self.transfer_failed(disabled.to_string());
        }
        Dirty::from(live || transfer)
    }

    pub(crate) fn sandbox_action_blocker(&self, transition: bool) -> Option<&'static str> {
        self.sandbox_admission_blocker(transition, false)
    }

    pub(crate) fn transfer_start_blocker(&self) -> Option<&'static str> {
        self.sandbox_admission_blocker(false, true)
    }

    fn sandbox_admission_blocker(&self, transition: bool, transfer: bool) -> Option<&'static str> {
        if let Some(reason) = self.sandbox_detached_action_blocker() {
            return Some(reason);
        }
        if self.has_lifecycle_work() || self.awaiting_input() || self.permission_mutation_pending()
        {
            return Some(
                "Wait for the active agent, permission or restore operation before a sandbox mutation",
            );
        }
        let workbench_busy = if transfer {
            self.workbench.backend_busy()
        } else {
            self.workbench.is_busy()
        };
        if workbench_busy {
            return Some(WORKBENCH_BUSY);
        }
        if (if transfer {
            self.workbench.blocks_transfer_start()
                || self
                    .parked_workbench
                    .as_ref()
                    .is_some_and(|workbench| workbench.blocks_workspace_change())
        } else {
            self.workbench_blocks_workspace_change()
        }) || (transition
            && (!self.input_box.is_empty()
                || !self.subagent_input_box.is_empty()
                || !self.subagent_drafts.is_empty()))
        {
            return Some(UNSAVED_DRAFT_ERR);
        }
        None
    }

    pub(crate) fn sandbox_detached_action_blocker(&self) -> Option<&'static str> {
        if self.sandbox_live.network_reply.is_some() {
            return Some("Wait for saved-network reconciliation before changing sandbox authority");
        }
        if self.sandbox_live.transfer.is_some() || self.sandbox_live.reply.is_some() {
            return Some(
                "Wait for sandbox operations and close the transfer connection before changing sandbox authority",
            );
        }
        if self.sandbox_manager.dirty() {
            return Some("Save or discard sandbox configuration edits first");
        }
        None
    }

    fn poll_sandbox_live(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        if let Some(receiver) = &self.sandbox_live.reply {
            match receiver.try_recv() {
                Ok(reply) => {
                    self.sandbox_live.reply = None;
                    if reply.scope.conversation == self.state.session.id {
                        self.sandbox_live.attachment = self.sandbox_manager.receive_live(reply);
                    }
                    self.sandbox_live.refresh_at = None;
                    dirty = Dirty::YES;
                }
                Err(TryRecvError::Disconnected) => {
                    self.sandbox_live.reply = None;
                    self.sandbox_manager.live_failed(
                        "Worker disconnected: outcome unknown. Reconcile; never replay Create."
                            .into(),
                    );
                    dirty = Dirty::YES;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &self.sandbox_live.snapshot {
            match receiver.try_recv() {
                Ok(reply) => {
                    self.sandbox_live.snapshot = None;
                    self.install_sandbox_snapshot(&reply.request, reply.snapshot);
                    dirty = Dirty::YES;
                }
                Err(TryRecvError::Disconnected) => {
                    self.sandbox_live.snapshot = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.sandbox_manager.is_open()
            && !self.sandbox_manager.pending()
            && self.sandbox_live.snapshot.is_none()
            && let Some(request) = self.sandbox_snapshot_request()
            && (self.sandbox_live.scope.as_ref() != Some(&request)
                || self
                    .sandbox_live
                    .refresh_at
                    .is_none_or(|at| Instant::now() >= at))
            && let Some(saved) = self.sandbox_manager.baseline()
        {
            self.sandbox_live.sequence += 1;
            self.sandbox_live.refresh_at = Some(Instant::now() + REFRESH_INTERVAL);
            self.sandbox_live.scope = Some(request.clone());
            self.sandbox_live.snapshot = Some(start_snapshot(
                request,
                saved,
                self.storage.clone(),
                self.sandbox_live.sequence,
                self.sandbox_live
                    .readiness
                    .as_ref()
                    .filter(|ready| ready())
                    .and_then(|_| self.state.session.workspace_binding().cloned()),
            ));
        }
        dirty
    }

    pub(crate) fn poll_sandbox(&mut self) -> Dirty {
        let dirty = self.poll_transfer() | self.poll_sandbox_live() | self.poll_network_reconcile();
        let Some(receiver) = &self.sandbox_reply else {
            return dirty;
        };
        match receiver.try_recv() {
            Ok(reply) => {
                self.sandbox_reply = None;
                let mut uncertain_save = None;
                if let Some((ticket, baseline)) = self.sandbox_live.network_save.take() {
                    let request = match &reply.result {
                        StoreResult::Saved(saved) if ticket == reply.ticket => {
                            NetworkReconcileRequest::committed(&baseline, saved.clone())
                        }
                        _ => None,
                    };
                    if let Some(request) = request {
                        self.sandbox_live.network_queue.push_back(request);
                        self.flash(NETWORK_SAVE_NOTICE.into());
                    } else {
                        let uncertain =
                            ticket != reply.ticket || reply.result.save_may_have_published();
                        if let Ok(mut gate) = self.sandbox_live.network_gate.lock() {
                            gate.pending = gate.pending.saturating_sub(1);
                            gate.unknown |= uncertain;
                        }
                        if uncertain {
                            uncertain_save = Some(NetworkReconcileReport {
                                revision: baseline.saved().revision().clone(),
                                instances: Vec::new(),
                                error: Some(NETWORK_SAVE_UNKNOWN.into()),
                                recovery: false,
                            });
                        }
                    }
                }
                let action = self.sandbox_manager.receive(reply);
                self.handle_sandbox_action(action);
                if let Some(report) = uncertain_save {
                    self.publish_network_report(report);
                }
                Dirty::YES
            }
            Err(TryRecvError::Empty) => dirty,
            Err(TryRecvError::Disconnected) => {
                self.sandbox_reply = None;
                self.sandbox_manager.disconnected();
                if let Some((_, baseline)) = self.sandbox_live.network_save.take() {
                    if let Ok(mut gate) = self.sandbox_live.network_gate.lock() {
                        gate.pending = gate.pending.saturating_sub(1);
                        gate.unknown = true;
                    }
                    self.publish_network_report(NetworkReconcileReport {
                        revision: baseline.saved().revision().clone(),
                        instances: Vec::new(),
                        error: Some(NETWORK_SAVE_UNKNOWN.into()),
                        recovery: false,
                    });
                }
                Dirty::YES
            }
        }
    }

    pub fn sandbox_network_reconciliation_pending(&self) -> bool {
        self.sandbox_live.network_save.is_some()
            || self.sandbox_live.network_reply.is_some()
            || !self.sandbox_live.network_queue.is_empty()
    }

    pub fn sandbox_network_reconciliation_report(&self) -> Option<&NetworkReconcileReport> {
        self.sandbox_live.network_report.as_deref()
    }

    pub(crate) fn sandbox_network_dispatch_blocker(&self) -> Option<&'static str> {
        let id = self.state.session.workspace_binding()?.sandbox_record()?;
        let gate = match self.sandbox_live.network_gate.lock() {
            Ok(gate) => gate,
            Err(_) => return Some(NETWORK_RECOVERY),
        };
        gate.blocker(id)
    }

    fn enqueue_network_reconcile(&mut self, request: NetworkReconcileRequest) {
        if let Ok(mut gate) = self.sandbox_live.network_gate.lock() {
            gate.pending += 1;
        }
        self.sandbox_live.network_queue.push_back(request);
    }

    fn receive_network_report(&mut self, report: NetworkReconcileReport) {
        if let Ok(mut gate) = self.sandbox_live.network_gate.lock() {
            gate.finish(&report);
        }
        self.publish_network_report(report);
    }

    fn publish_network_report(&mut self, report: NetworkReconcileReport) {
        let report = Arc::new(report);
        if let Ok(mut gate) = self.sandbox_live.network_gate.lock() {
            gate.latest_report = Some(report);
        }
        let _ = self.sync_network_report();
    }

    fn sync_network_report(&mut self) -> Dirty {
        let report = self
            .sandbox_live
            .network_gate
            .lock()
            .ok()
            .and_then(|gate| gate.latest_report.clone());
        let Some(report) = report else {
            return Dirty::NO;
        };
        if self
            .sandbox_live
            .network_report
            .as_ref()
            .is_some_and(|previous| Arc::ptr_eq(previous, &report))
        {
            return Dirty::NO;
        }
        let summary = report.summary();
        self.sandbox_manager.receive_network_report(summary.clone());
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Notice, summary));
        self.flash(report.error.clone().unwrap_or_else(|| "Saved-network reconciliation finished. Results are in /sandbox and the conversation; saved does not necessarily mean enforced.".into()));
        self.sandbox_live.network_report = Some(report);
        Dirty::YES
    }

    fn poll_network_reconcile(&mut self) -> Dirty {
        let mut dirty = self.sync_network_report();
        if let Some((revision, receiver)) = &self.sandbox_live.network_reply {
            match receiver.try_recv() {
                Ok(report) => {
                    self.sandbox_live.network_reply = None;
                    self.receive_network_report(report);
                    self.sandbox_live.refresh_at = None;
                    dirty = Dirty::YES;
                }
                Err(TryRecvError::Disconnected) => {
                    let report = NetworkReconcileReport {
                        revision: revision.clone(),
                        instances: Vec::new(),
                        error: Some("Worker disconnected: enforcement outcome unknown. Inspect affected instances before any retry. No automatic VM teardown.".into()),
                        recovery: false,
                    };
                    self.sandbox_live.network_reply = None;
                    self.receive_network_report(report);
                    dirty = Dirty::YES;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let can_start = self.sandbox_live.reply.is_none()
            && self.sandbox_live.network_reply.is_none()
            && self.sandbox_live.transfer.is_none()
            && !self.sandbox_live.network_queue.is_empty()
            && self.sandbox_live.network_gate.lock().is_ok_and(|mut gate| {
                if gate.running {
                    false
                } else {
                    gate.running = true;
                    true
                }
            });
        if can_start && let Some(request) = self.sandbox_live.network_queue.pop_front() {
            self.sandbox_live.network_reply = Some((
                request.saved.saved().revision().clone(),
                start_network_reconcile(request, self.storage.clone()),
            ));
            dirty = Dirty::YES;
        }
        dirty
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::tests::remote_workspace_session;
    use super::super::tests::test_app;
    use super::UNSAVED_DRAFT_ERR;
    #[cfg(unix)]
    use crate::AppSession;
    use crate::app::Msg;
    use crate::components::Overlay;
    use crate::components::Status;
    use crate::components::keybindings::key;
    #[cfg(unix)]
    use crate::components::permission_prompt::PermissionDecision;
    #[cfg(unix)]
    use crate::components::sandbox_manager::SandboxAction;
    use crate::components::sandbox_manager::SandboxView;
    #[cfg(unix)]
    use crate::components::sandbox_manager::tests::{fixture, live_instance};
    use crate::repaint::Dirty;
    #[cfg(unix)]
    use crate::sandbox::transfer::{
        TransferCommand, TransferLink, TransferScope, transfer_permissions,
    };
    #[cfg(unix)]
    use crate::sandbox::{
        LiveOperation, LiveOutcome, LiveReply, NETWORK_RECOVERY, NETWORK_SAVE_UNKNOWN,
        NetworkReconcileReport, NetworkReconcileRequest, SandboxSnapshot, SnapshotReply,
        SnapshotState, StoreReply, StoreResult, StoreTicket, execute_store_effect,
    };
    #[cfg(unix)]
    use caudra_agent::permissions::{PermissionAnswer, PermissionManager, PluginRuleStore};
    #[cfg(unix)]
    use caudra_agent::workspace_transfer::{
        LocalAccess, LocalRootIdentity, RemoteRootIdentity, TransferAuthorization, TransferRoots,
    };
    #[cfg(unix)]
    use caudra_agent::{AgentEvent, CancelToken, EventSender};
    #[cfg(unix)]
    use caudra_config::{
        PermissionsConfig,
        sandbox::{
            SandboxName,
            persistence::{SandboxStore, SandboxStoreError},
        },
    };
    use caudra_providers::{ImageMediaType, ImageSource};
    #[cfg(unix)]
    use caudra_storage::{
        id::CaudraId, private_file::PrivateFileError, workspace_binding::StoredWorkspaceBinding,
    };
    use caudra_workbench::{Workbench, WorkbenchStyles};
    #[cfg(unix)]
    use caudra_workcell::NativeTransferAuthorization;
    #[cfg(unix)]
    use caudra_workspace::WorkspacePath;
    use crossterm::event::KeyEvent;
    #[cfg(unix)]
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    #[cfg(unix)]
    use futures_lite::future;
    #[cfg(unix)]
    use ratatui::{Terminal, backend::TestBackend};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::Mutex;
    #[cfg(unix)]
    use std::sync::atomic::Ordering;
    #[cfg(unix)]
    use std::time::Duration;
    #[cfg(unix)]
    use tempfile::TempDir;
    use test_case::test_case;

    const UNSAVED: &str = "unsent local draft";
    const IMAGE_DATA: &str = "dGVzdA==";
    const SAVED_BYTES: &str = "saved local file\n";
    #[cfg(unix)]
    const PERMISSIONS: &str = ".caudra/permissions.toml";
    #[cfg(unix)]
    const DENY_TRANSFER: &str = "[workspace_transfer]\ndeny = true\n";
    #[cfg(unix)]
    const DENY_DEFAULT: &str = "default = 'deny'\n";
    #[cfg(unix)]
    const TRANSFER_TEST_DONE: &str = "test authorization complete";
    #[cfg(unix)]
    const PERMISSION_TIMEOUT: Duration = Duration::from_secs(10);
    #[cfg(unix)]
    const DOTENV_KEY: &str = "CAUDRA_TRANSFER_TEST_NO_DOTENV";
    #[cfg(unix)]
    const PRIVATE_MODE: u32 = 0o700;
    #[cfg(unix)]
    const SANDBOXES: &str = "sandboxes.toml";
    #[cfg(unix)]
    static TRANSFER_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    const CREATE_NAME: &str = "reviewed-create";
    #[cfg(unix)]
    const CREATE_REPORT: &str = "Fake lifecycle report: no VM was created";
    #[cfg(unix)]
    const CREATE_ERROR: &str = "Fake lifecycle failure: outcome unknown; Reconcile before retrying";
    #[cfg(unix)]
    const NEW_NETWORK: &str = "new-network";

    #[test_case(false; "idle_with_unsent_draft")]
    #[test_case(true; "active_agent_with_unsent_draft")]
    fn detached_controls_do_not_gate_or_cancel_unrelated_current_work(streaming: bool) {
        let mut app = test_app();
        app.input_box.set_input(UNSAVED.into());
        if streaming {
            app.status = Status::Streaming;
        }
        assert!(app.sandbox_action_blocker(true).is_some());
        assert_eq!(app.sandbox_detached_action_blocker(), None);
        assert_eq!(app.active_input_text(), UNSAVED);
        assert_eq!(app.status == Status::Streaming, streaming);
        assert!(app.sandbox_live.queued.is_none());
    }

    #[cfg(unix)]
    #[test_case(false; "lifecycle_worker")]
    #[test_case(true; "network_worker")]
    fn detached_controls_still_serialize_sandbox_workers(network: bool) {
        let (_directory, store, manager) = fixture();
        let mut app = test_app();
        app.sandbox_manager = manager;
        if network {
            let (_sender, receiver) = flume::bounded(1);
            app.sandbox_live.network_reply =
                Some((store.load().unwrap().saved().revision().clone(), receiver));
        } else {
            let (_sender, receiver) = flume::bounded(1);
            app.sandbox_live.reply = Some(receiver);
        }
        assert!(app.sandbox_detached_action_blocker().is_some());
    }

    #[cfg(unix)]
    #[test_case(PrivateFileError::DurabilityUnknown, true; "published_but_not_durable")]
    #[test_case(PrivateFileError::Busy, false; "prepublication_lock_failure")]
    #[test_case(PrivateFileError::Permissions { path: SANDBOXES.into(), mode: PRIVATE_MODE }, false; "prepublication_permission_failure")]
    fn network_save_failure_preserves_uncertain_publication_through_manager_close(
        error: PrivateFileError,
        published: bool,
    ) {
        let (_directory, store, mut manager) = fixture();
        let baseline = Arc::new(store.load().unwrap());
        let SandboxAction::Store { ticket, .. } = manager.handle_key(key::SAVE.to_key_event())
        else {
            panic!("save ticket required");
        };
        if published {
            let mut draft = baseline.draft();
            draft
                .networks
                .insert(SandboxName::parse(NEW_NETWORK).unwrap(), Default::default());
            store.save(&baseline, &draft).unwrap();
        }
        let mut app = test_app();
        app.sandbox_manager = manager;
        app.sandbox_live.network_save = Some((ticket.clone(), baseline.clone()));
        app.sandbox_live.network_gate.lock().unwrap().pending = 1;
        let (sender, receiver) = flume::bounded(1);
        app.sandbox_reply = Some(receiver);
        sender
            .send(StoreReply {
                ticket,
                result: StoreResult::Failed(SandboxStoreError::File(error)),
            })
            .unwrap();
        assert_eq!(app.poll_sandbox(), Dirty::YES);
        app.sandbox_manager.close();
        assert!(!app.sandbox_manager.is_open());
        assert!(!app.sandbox_network_reconciliation_pending());
        assert!(app.sandbox_live.network_queue.is_empty());
        assert!(app.sandbox_live.network_reply.is_none());
        let id = CaudraId::generate();
        {
            let gate = app.sandbox_live.network_gate.lock().unwrap();
            assert_eq!(gate.pending, 0);
            assert_eq!(gate.unknown, published);
            assert_eq!(gate.blocker(id), published.then_some(NETWORK_RECOVERY));
        }
        assert_eq!(
            store.load().unwrap().saved().revision() != baseline.saved().revision(),
            published
        );
        if published {
            assert_eq!(
                app.sandbox_network_reconciliation_report()
                    .unwrap()
                    .error
                    .as_deref(),
                Some(NETWORK_SAVE_UNKNOWN)
            );
            assert!(!app.sandbox_manager.pending());
            app.open_sandbox("instances");
            assert!(network_report_frame(&mut app).contains("Do not retry Save"));
            assert!(app.sandbox_live.network_gate.lock().unwrap().unknown);
            app.open_sandbox("reconcile-network");
            let recovery = app.sandbox_live.network_queue.front().unwrap();
            assert!(recovery.recovery);
            assert!(app.sandbox_reply.is_none());
        } else {
            assert!(app.sandbox_network_reconciliation_report().is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn network_reports_follow_shared_gate_into_other_sessions_and_reopened_managers() {
        let (_directory, store, manager) = fixture();
        let mut origin = test_app();
        let mut observer = test_app();
        observer.sandbox_manager = manager;
        observer.sandbox_manager.close();
        observer.sandbox_live.network_gate = origin.sandbox_live.network_gate.clone();
        origin.sandbox_live.network_gate.lock().unwrap().unknown = true;
        origin.publish_network_report(NetworkReconcileReport {
            revision: store.load().unwrap().saved().revision().clone(),
            instances: Vec::new(),
            error: Some(NETWORK_SAVE_UNKNOWN.into()),
            recovery: false,
        });
        assert_eq!(observer.sync_network_report(), Dirty::YES);
        assert_eq!(observer.sync_network_report(), Dirty::NO);
        assert!(!observer.sandbox_manager.is_open());
        observer.open_sandbox("instances");
        assert!(network_report_frame(&mut observer).contains("Do not retry Save"));
        let shared = observer
            .sandbox_live
            .network_gate
            .lock()
            .unwrap()
            .latest_report
            .clone()
            .unwrap();
        assert!(Arc::ptr_eq(
            observer.sandbox_live.network_report.as_ref().unwrap(),
            &shared
        ));
        origin.receive_network_report(NetworkReconcileReport {
            revision: shared.revision.clone(),
            instances: Vec::new(),
            error: None,
            recovery: true,
        });
        assert_eq!(observer.sync_network_report(), Dirty::YES);
        assert!(
            observer
                .sandbox_network_reconciliation_report()
                .unwrap()
                .error
                .is_none()
        );
        assert!(!observer.sandbox_live.network_gate.lock().unwrap().unknown);
        assert!(!network_report_frame(&mut observer).contains("Do not retry Save"));
    }

    #[cfg(unix)]
    fn network_report_frame(app: &mut super::App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 60)).unwrap();
        terminal.draw(|frame| app.view(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(140)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(unix)]
    #[test_case(true; "matching_commit")]
    #[test_case(false; "stale_ticket")]
    fn network_save_queues_only_matching_committed_reply(matching: bool) {
        let (_directory, store, _manager) = fixture();
        let baseline = Arc::new(store.load().unwrap());
        let mut draft = baseline.draft();
        let name = SandboxName::parse(NEW_NETWORK).unwrap();
        draft.networks.insert(name, Default::default());
        let saved = Arc::new(store.save(&baseline, &draft).unwrap());
        let ticket = StoreTicket {
            session: 1,
            operation: 1,
            draft_revision: 1,
        };
        let mut reply_ticket = ticket.clone();
        if !matching {
            reply_ticket.operation += 1;
        }
        let mut app = test_app();
        app.sandbox_live.network_save = Some((ticket, baseline));
        app.sandbox_live.network_gate.lock().unwrap().pending = 1;
        let (sender, receiver) = flume::bounded(1);
        app.sandbox_reply = Some(receiver);
        sender
            .send(StoreReply {
                ticket: reply_ticket,
                result: StoreResult::Saved(saved.clone()),
            })
            .unwrap();
        assert_eq!(app.poll_sandbox(), Dirty::YES);
        assert_eq!(app.sandbox_network_reconciliation_pending(), matching);
        assert_eq!(
            app.sandbox_live.network_gate.lock().unwrap().pending,
            usize::from(matching)
        );
        if matching {
            assert_eq!(
                app.sandbox_live
                    .network_queue
                    .front()
                    .unwrap()
                    .saved
                    .saved()
                    .revision(),
                saved.saved().revision()
            );
        }
        assert!(app.sandbox_live.network_reply.is_none());
        assert!(app.sandbox_live.queued.is_none());
        assert!(app.workspace_session.is_none());
    }

    #[cfg(unix)]
    #[test_case(false; "completed")]
    #[test_case(true; "unknown_disconnect")]
    fn network_worker_completion_never_replays_or_changes_workspace(disconnected: bool) {
        let (_directory, store, _manager) = fixture();
        let revision = store.load().unwrap().saved().revision().clone();
        let mut app = test_app();
        let (sender, receiver) = flume::bounded(1);
        app.sandbox_live.network_reply = Some((revision.clone(), receiver));
        assert!(app.sandbox_action_blocker(false).is_some());
        if !disconnected {
            sender
                .send(NetworkReconcileReport {
                    revision: revision.clone(),
                    instances: Vec::new(),
                    error: None,
                    recovery: false,
                })
                .unwrap();
        }
        drop(sender);
        assert_eq!(app.poll_network_reconcile(), Dirty::YES);
        let report = app.sandbox_network_reconciliation_report().unwrap();
        assert_eq!(report.revision, revision);
        assert_eq!(report.error.is_some(), disconnected);
        assert!(!app.sandbox_network_reconciliation_pending());
        assert_eq!(app.sandbox_action_blocker(false), None);
        assert!(app.sandbox_live.queued.is_none());
        assert!(app.workspace_session.is_none());
        assert!(!app.sandbox_manager.is_open());
        assert_eq!(app.sandbox_network_dispatch_blocker(), None);
    }

    #[cfg(unix)]
    #[test]
    fn network_jobs_wait_for_shared_worker_without_blocking_local_execution() {
        let (_directory, store, _manager) = fixture();
        let mut app = test_app();
        app.sandbox_live.network_gate.lock().unwrap().running = true;
        app.enqueue_network_reconcile(NetworkReconcileRequest {
            saved: Arc::new(store.load().unwrap()),
            networks: Default::default(),
            recovery: true,
        });
        assert_eq!(app.poll_network_reconcile(), Dirty::NO);
        assert_eq!(app.sandbox_live.network_queue.len(), 1);
        assert!(app.sandbox_live.network_reply.is_none());
        assert_eq!(app.sandbox_network_dispatch_blocker(), None);
    }

    #[cfg(unix)]
    fn sandbox_frame(app: &mut super::App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.view(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[cfg(unix)]
    fn sandbox_key(app: &mut super::App, code: KeyCode) {
        assert!(
            app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
                .is_empty()
        );
    }

    #[cfg(unix)]
    fn create_app() -> (TempDir, super::App) {
        let (directory, _, manager) = fixture();
        let mut app = test_app();
        app.sandbox_manager = manager;
        app.sandbox_manager
            .open(app.state.session.id, SandboxView::Profiles);
        live_instance(&mut app.sandbox_manager, false);
        app.sandbox_manager
            .open(app.state.session.id, SandboxView::Profiles);
        sandbox_key(&mut app, KeyCode::Char('v'));
        sandbox_key(&mut app, KeyCode::Tab);
        app.update(Msg::Paste(CREATE_NAME.into()));
        (directory, app)
    }

    #[cfg(unix)]
    #[test_case(-2, "> a.qcow2"; "aggregated_down")]
    #[test_case(2, "> c.qcow2"; "aggregated_up")]
    fn host_picker_receives_aggregated_scroll(delta: i32, selected: &str) {
        const SOURCE_FIELD: usize = 5;
        let (directory, _, manager) = fixture();
        let host = directory.path().join("images");
        fs::create_dir(&host).unwrap();
        for name in ["a.qcow2", "b.qcow2", "c.qcow2"] {
            fs::write(host.join(name), []).unwrap();
        }
        let mut app = test_app();
        app.sandbox_manager = manager;
        app.sandbox_manager
            .open(app.state.session.id, SandboxView::Images);
        sandbox_key(&mut app, KeyCode::Char('i'));
        for _ in 0..SOURCE_FIELD {
            sandbox_key(&mut app, KeyCode::Tab);
        }
        app.update(Msg::Paste(host.to_string_lossy().into_owned()));
        sandbox_key(&mut app, KeyCode::F(2));
        if delta > 0 {
            sandbox_key(&mut app, KeyCode::End);
        }
        let frame = sandbox_frame(&mut app);
        let row = frame
            .iter()
            .position(|line| line.contains("a.qcow2"))
            .unwrap();
        let column = frame[row].find("a.qcow2").unwrap();
        assert!(
            app.update(Msg::Scroll {
                column: column as u16,
                row: row as u16,
                delta
            })
            .is_empty()
        );
        assert!(
            sandbox_frame(&mut app)
                .iter()
                .any(|line| line.contains(selected))
        );
        assert!(app.sandbox_live.queued.is_none());
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(
            sandbox_frame(&mut app)
                .iter()
                .any(|line| line.contains("Host qcow2"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_review_selection_copies_without_accepting_action() {
        let (_directory, mut app) = create_app();
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        let review = sandbox_frame(&mut app);
        let source_row = review
            .iter()
            .position(|line| line.contains(CREATE_NAME))
            .unwrap();
        let source_column = review[source_row].find(CREATE_NAME).unwrap();
        let button_row = review
            .iter()
            .position(|line| line.contains("Accept reviewed action"))
            .unwrap();
        let button_column = review[button_row].find("Accept reviewed action").unwrap();
        for (kind, row, column) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                source_row,
                source_column,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                button_row,
                button_column,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                button_row,
                button_column,
            ),
        ] {
            assert!(
                app.update(Msg::Mouse(MouseEvent {
                    kind,
                    column: column as u16,
                    row: row as u16,
                    modifiers: KeyModifiers::NONE
                }))
                .is_empty()
            );
        }
        assert!(app.sandbox_live.queued.is_none());
        assert!(!app.sandbox_manager.pending());
        assert!(app.selection_state.is_none());
        for character in ['a', 'c'] {
            assert!(
                app.update(Msg::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::CONTROL
                )))
                .is_empty()
            );
        }
        assert!(app.status_bar.flash_text().is_some());
        assert!(app.sandbox_live.queued.is_none());
        assert!(
            sandbox_frame(&mut app)
                .iter()
                .any(|line| line.contains("Accept reviewed action"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_masked_credential_mouse_and_keyboard_never_reach_clipboard() {
        const SECRET: &str = "sandbox-selection-secret";
        let (_directory, _, manager) = fixture();
        let mut app = test_app();
        app.sandbox_manager = manager;
        sandbox_key(&mut app, KeyCode::Char('4'));
        sandbox_key(&mut app, KeyCode::Char('k'));
        sandbox_key(&mut app, KeyCode::Tab);
        app.update(Msg::Paste(SECRET.into()));
        let frame = sandbox_frame(&mut app);
        assert!(!frame.join("\n").contains(SECRET));
        let row = frame.iter().position(|line| line.contains("***")).unwrap();
        let column = frame[row].find("***").unwrap();
        let flash = app.status_bar.flash_text().map(str::to_owned);
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), column),
            (
                MouseEventKind::Drag(MouseButton::Left),
                column + SECRET.len(),
            ),
            (MouseEventKind::Up(MouseButton::Left), column + SECRET.len()),
        ] {
            app.update(Msg::Mouse(MouseEvent {
                kind,
                column: column as u16,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            }));
        }
        for character in ['a', 'c'] {
            app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::CONTROL,
            )));
        }
        assert_eq!(app.status_bar.flash_text(), flash.as_deref());
        assert!(app.sandbox_live.queued.is_none());
        assert!(app.sandbox_manager.is_open());
    }

    #[cfg(unix)]
    #[test_case(false, false; "key_pending")]
    #[test_case(true, false; "mouse_pending")]
    #[test_case(false, true; "key_gate_rejection")]
    #[test_case(true, true; "mouse_gate_rejection")]
    fn create_acceptance_leaves_wizard_and_can_exit(mouse: bool, rejected: bool) {
        let (_directory, mut app) = create_app();
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        let review = sandbox_frame(&mut app);
        assert!(review.iter().any(|line| line.contains(CREATE_NAME)));
        assert!(app.sandbox_live.queued.is_none());
        if mouse {
            let row = review
                .iter()
                .position(|line| line.contains("Accept reviewed action"))
                .unwrap();
            let column = review[row].find("Accept reviewed action").unwrap();
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                app.update(Msg::Mouse(MouseEvent {
                    kind,
                    column: column as u16,
                    row: row as u16,
                    modifiers: KeyModifiers::NONE,
                }));
            }
        } else {
            sandbox_key(&mut app, KeyCode::Char('s'));
        }
        let request = app.sandbox_live.queued.take().expect("accepted request");
        assert!(matches!(request.operation, LiveOperation::Create { .. }));
        if rejected {
            app.status = Status::Streaming;
            let reason = app.sandbox_action_blocker(false).unwrap();
            app.sandbox_failed(reason.into());
            assert!(sandbox_frame(&mut app).join("\n").contains("active agent"));
        }
        assert!(
            !sandbox_frame(&mut app)
                .join("\n")
                .contains("New instance name")
        );
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        sandbox_key(&mut app, KeyCode::Char('s'));
        assert!(app.sandbox_live.queued.is_none());
        app.restoring.store(true, Ordering::Relaxed);
        sandbox_key(&mut app, KeyCode::Esc);
        if app.sandbox_manager.is_open() {
            sandbox_key(&mut app, KeyCode::Esc);
        }
        assert!(!app.sandbox_manager.is_open());
        if !rejected {
            assert!(app.sandbox_manager.pending());
            let (sender, receiver) = flume::bounded(1);
            app.sandbox_live.reply = Some(receiver);
            sender
                .send(LiveReply {
                    ticket: request.ticket,
                    scope: request.scope,
                    result: Ok(LiveOutcome::Report(CREATE_REPORT.into())),
                })
                .unwrap();
            assert_eq!(app.poll_sandbox_live(), Dirty::YES);
            assert!(!app.sandbox_manager.pending());
            assert!(!app.sandbox_manager.is_open());
        }
        assert!(app.workspace_session.is_none());
    }

    #[cfg(unix)]
    #[test_case(false; "report")]
    #[test_case(true; "failure")]
    fn create_worker_result_is_visible_and_back_does_not_resubmit(failed: bool) {
        let (_directory, mut app) = create_app();
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        sandbox_key(&mut app, KeyCode::Char('s'));
        let request = app.sandbox_live.queued.take().unwrap();
        let (snapshot_sender, snapshot_receiver) = flume::bounded(1);
        app.sandbox_live.snapshot = Some(snapshot_receiver);
        let (sender, receiver) = flume::bounded(1);
        app.sandbox_live.reply = Some(receiver);
        let report = if failed { CREATE_ERROR } else { CREATE_REPORT };
        sender
            .send(LiveReply {
                ticket: request.ticket,
                scope: request.scope,
                result: if failed {
                    Err(report.into())
                } else {
                    Ok(LiveOutcome::Report(report.into()))
                },
            })
            .unwrap();
        assert_eq!(app.poll_sandbox_live(), Dirty::YES);
        assert!(!app.sandbox_manager.pending());
        assert!(sandbox_frame(&mut app).join("\n").contains(report));
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        sandbox_key(&mut app, KeyCode::Char('s'));
        assert!(app.sandbox_live.queued.is_none());
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(app.sandbox_manager.is_open());
        assert!(
            !sandbox_frame(&mut app)
                .join("\n")
                .contains("New instance name")
        );
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(!app.sandbox_manager.is_open());
        drop(snapshot_sender);
    }

    #[cfg(unix)]
    #[test_case(false; "open")]
    #[test_case(true; "closed")]
    fn create_worker_disconnect_never_restores_submittable_form(closed: bool) {
        let (_directory, mut app) = create_app();
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        sandbox_key(&mut app, KeyCode::Char('s'));
        let _request = app.sandbox_live.queued.take().unwrap();
        let (snapshot_sender, snapshot_receiver) = flume::bounded(1);
        app.sandbox_live.snapshot = Some(snapshot_receiver);
        let (sender, receiver) = flume::bounded(1);
        app.sandbox_live.reply = Some(receiver);
        if closed {
            sandbox_key(&mut app, KeyCode::Esc);
        }
        drop(sender);
        assert_eq!(app.poll_sandbox_live(), Dirty::YES);
        assert_eq!(app.sandbox_manager.is_open(), !closed);
        assert!(!app.sandbox_manager.pending());
        if !closed {
            let frame = sandbox_frame(&mut app).join("\n");
            assert!(frame.contains("Worker disconnected"));
            assert!(!frame.contains("New instance name"));
        }
        drop(snapshot_sender);
    }

    #[cfg(unix)]
    #[test_case(false; "current_completion")]
    #[test_case(true; "stale_completion")]
    fn closed_create_late_snapshot_and_reply_never_reopen(stale: bool) {
        let (_directory, mut app) = create_app();
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        sandbox_key(&mut app, KeyCode::Char('s'));
        let request = app.sandbox_live.queued.take().unwrap();
        let (snapshot_sender, snapshot_receiver) = flume::bounded(1);
        app.sandbox_live.snapshot = Some(snapshot_receiver);
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(!app.sandbox_manager.is_open());
        let (sender, receiver) = flume::bounded(1);
        app.sandbox_live.reply = Some(receiver);
        let mut ticket = request.ticket;
        if stale {
            ticket.operation += 1;
        }
        sender
            .send(LiveReply {
                ticket,
                scope: request.scope.clone(),
                result: Ok(LiveOutcome::Report(CREATE_REPORT.into())),
            })
            .unwrap();
        snapshot_sender
            .send(SnapshotReply {
                request: request.scope,
                snapshot: SandboxSnapshot {
                    sequence: 2,
                    instances: SnapshotState::Ready(Vec::new()),
                    providers: Default::default(),
                    failures: Default::default(),
                    credentials: Vec::new(),
                },
            })
            .unwrap();
        assert_eq!(app.poll_sandbox_live(), Dirty::YES);
        assert_eq!(app.sandbox_manager.pending(), stale);
        assert!(!app.sandbox_manager.is_open());
        assert!(app.sandbox_live.queued.is_none());
        assert!(app.sandbox_live.attachment.is_none());
        assert!(!sandbox_frame(&mut app).join("\n").contains("Sandboxes"));
    }

    #[cfg(unix)]
    #[test_case(false; "keyboard_discard")]
    #[test_case(true; "mouse_discard")]
    fn create_draft_back_retains_and_explicit_discard_drops_it(mouse: bool) {
        let (_directory, mut app) = create_app();
        app.update(Msg::Key(key::SANDBOX_APPLY.to_key_event()));
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(app.sandbox_live.queued.is_none());
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(app.sandbox_manager.is_open());
        assert!(
            !sandbox_frame(&mut app)
                .join("\n")
                .contains("New instance name")
        );
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(!app.sandbox_manager.is_open());
        app.sandbox_manager
            .open(app.state.session.id, SandboxView::Profiles);
        sandbox_key(&mut app, KeyCode::Char('v'));
        let frame = sandbox_frame(&mut app);
        assert!(frame.iter().any(|line| line.contains(CREATE_NAME)));
        if mouse {
            let row = frame
                .iter()
                .position(|line| line.contains("Discard draft"))
                .unwrap();
            let column = frame[row].find("Discard draft").unwrap();
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                app.update(Msg::Mouse(MouseEvent {
                    kind,
                    column: column as u16,
                    row: row as u16,
                    modifiers: KeyModifiers::NONE,
                }));
            }
        } else {
            sandbox_key(&mut app, KeyCode::F(6));
        }
        assert!(app.sandbox_manager.is_open());
        assert!(
            !sandbox_frame(&mut app)
                .join("\n")
                .contains("New instance name")
        );
        assert!(app.sandbox_live.queued.is_none());
        sandbox_key(&mut app, KeyCode::Char('v'));
        assert!(!sandbox_frame(&mut app).join("\n").contains(CREATE_NAME));
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(app.sandbox_manager.is_open());
        sandbox_key(&mut app, KeyCode::Esc);
        assert!(!app.sandbox_manager.is_open());
    }

    #[cfg(unix)]
    #[test_case(false, None, false; "project_a_active_remembers_only_b")]
    #[test_case(true, None, false; "remote_active_remembers_only_b")]
    #[test_case(false, Some(DENY_TRANSFER), false; "project_a_cannot_override_b_deny")]
    #[test_case(true, Some(DENY_DEFAULT), false; "remote_cannot_override_b_default_deny")]
    #[test_case(true, None, true; "remote_cannot_override_b_persisted_deny")]
    fn transfer_permissions_belong_to_selected_host_root(
        remote_active: bool,
        configured_deny: Option<&str>,
        persisted_deny: bool,
    ) {
        let _serial = TRANSFER_TEST_LOCK.lock().unwrap();
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        fs::set_permissions(config.path(), fs::Permissions::from_mode(PRIVATE_MODE)).unwrap();
        let mut app = test_app();
        app.permissions = Arc::new(PermissionManager::new_persistent_in(
            PermissionsConfig {
                yolo: true,
                ..Default::default()
            },
            root_a.path().into(),
            Arc::new(PluginRuleStore::default()),
            app.storage.clone(),
        ));
        let active = app.permissions.clone();
        let remote = remote_workspace_session();
        let binding = StoredWorkspaceBinding::new_with_cursor(
            remote.binding().clone(),
            remote.cursor().clone(),
            None,
        )
        .unwrap()
        .with_sandbox_record(CaudraId::generate())
        .unwrap();
        app.state.session = Arc::new(AppSession::new_with_workspace("test", ".", binding.clone()));
        app.sandbox_live.name = Some(SandboxName::parse("test").unwrap());
        app.sandbox_live.readiness = Some(Arc::new(|| true));
        app.sync_transfer_availability();
        assert!(app.workbench.open_transfer());
        if remote_active {
            app.workspace_session = Some(remote.clone());
        }
        let roots = TransferRoots {
            local: LocalRootIdentity::capture(root_b.path()).unwrap(),
            remote: RemoteRootIdentity {
                binding: remote.binding().clone(),
                cursor: remote.cursor().clone(),
                cwd: WorkspacePath::root(),
            },
        };
        fs::create_dir(root_b.path().join(".caudra")).unwrap();
        fs::write(
            root_b.path().join(".caudra/.env"),
            format!("{DOTENV_KEY}=loaded\n"),
        )
        .unwrap();
        if let Some(configuration) = configured_deny {
            fs::write(root_b.path().join(PERMISSIONS), configuration).unwrap();
        }
        if persisted_deny {
            let manager = Arc::new(transfer_permissions(root_b.path().into(), &app.storage));
            let (events, received) = flume::unbounded();
            let auth = NativeTransferAuthorization::new(
                manager.clone(),
                EventSender::new(events, 0),
                CancelToken::none(),
                Arc::new(|| Ok(())),
            );
            smol::block_on(async {
                let path = WorkspacePath::new("file.txt").unwrap();
                let mut request = Box::pin(auth.local(&roots, &path, LocalAccess::Write));
                assert!(future::poll_once(&mut request).await.is_none());
                let AgentEvent::PermissionRequest(requested) = received.try_recv().unwrap().event
                else {
                    panic!("native prompt required")
                };
                assert!(manager.answer(&requested.id, PermissionAnswer::DenyAlwaysLocal));
                assert!(request.await.is_err());
            });
        }
        let SandboxAction::Store { ticket, effect } = app
            .sandbox_manager
            .open(app.state.session.id, SandboxView::Instances)
        else {
            panic!("configuration load required")
        };
        app.sandbox_manager.receive(StoreReply {
            ticket,
            result: execute_store_effect(
                Ok(SandboxStore::from_config_dir(config.path()).unwrap()),
                effect,
            ),
        });
        let configuration_revision = app
            .sandbox_snapshot_request()
            .unwrap()
            .configuration_revision;
        app.sandbox_manager.close();
        let scope = TransferScope {
            conversation: app.state.session.id,
            binding,
            name: SandboxName::parse("test").unwrap(),
            instance_revision: configuration_revision.clone(),
            configuration_revision,
            generation: app.workbench.transfer_generation(),
        };
        let (ready, waiting) = flume::bounded(1);
        let (finished, result) = flume::bounded(1);
        app.sandbox_live.transfer_connector = Some(Arc::new(move |link, host| {
            assert_eq!(link.local_root, roots.local.canonical_path());
            assert_eq!(host.permissions.project_cwd(), link.local_root);
            let token = host.cancel.clone();
            let auth = NativeTransferAuthorization::new(
                host.permissions,
                host.permission_events,
                host.cancel,
                host.validity,
            );
            smol::block_on(async {
                let path = WorkspacePath::new("file.txt").unwrap();
                let mut request = Box::pin(auth.local(&roots, &path, LocalAccess::Write));
                let immediate = future::poll_once(&mut request).await;
                ready.send(immediate.is_none()).unwrap();
                let outcome = match immediate {
                    Some(result) => result,
                    None => request.await,
                };
                finished.send(outcome.is_ok()).unwrap();
                token.cancelled().await;
            });
            Err(TRANSFER_TEST_DONE.into())
        }));
        app.start_transfer(TransferCommand::Open {
            link: Box::new(TransferLink {
                name: SandboxName::parse("test").unwrap(),
                instance_revision: scope.configuration_revision.clone(),
                configuration_revision: scope.configuration_revision.clone(),
                local_root: root_b.path().into(),
                remote_root: WorkspacePath::root(),
                attached_binding: Some(scope.binding.clone()),
                include_ignored: false,
                skip_dotfiles: false,
            }),
            scope: Box::new(scope),
        });
        let denied = configured_deny.is_some() || persisted_deny;
        assert_eq!(waiting.recv_timeout(PERMISSION_TIMEOUT).unwrap(), !denied);
        let _ = app.poll_transfer();
        if !denied {
            let request_id = app.permission_prompt.request_id().unwrap().to_owned();
            assert!(active.pending_request(&request_id).is_none());
            app.apply_permission_decision(PermissionDecision {
                request_id,
                answer: PermissionAnswer::AllowAlwaysLocal,
            });
            app.finish_permission_jobs();
            assert!(!app.permission_prompt.is_open());
        } else {
            assert!(!app.permission_prompt.is_open());
        }
        assert_eq!(result.recv_timeout(PERMISSION_TIMEOUT).unwrap(), !denied);
        assert!(Arc::ptr_eq(&active, &app.permissions));
        assert!(active.structured_rule_inventory().unwrap().is_empty());
        let reopened = transfer_permissions(root_b.path().into(), &app.storage);
        assert_eq!(
            reopened.structured_rule_inventory().unwrap().len(),
            usize::from(configured_deny.is_none())
        );
        assert!(std::env::var_os(DOTENV_KEY).is_none());
        app.sandbox_live.transfer.take().unwrap().finish();
    }

    #[test]
    fn sandbox_overlay_paste_cannot_dirty_the_underlying_local_editor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local.txt");
        fs::write(&path, SAVED_BYTES).unwrap();
        let mut app = test_app();
        app.workbench = Workbench::open_local_source(
            WorkbenchStyles::default(),
            &path,
            None,
            &(),
            |_, _, _| Ok(()),
        )
        .unwrap();
        assert!(!app.workbench.blocks_workspace_change());
        app.sandbox_manager
            .open(app.state.session.id, SandboxView::Instances);
        assert!(app.update(Msg::Paste(UNSAVED.into())).is_empty());
        assert!(!app.workbench.blocks_workspace_change());
        assert_eq!(fs::read_to_string(path).unwrap(), SAVED_BYTES);
        assert!(app.active_input_text().is_empty());
    }

    #[test]
    fn transfer_gate_preserves_dirty_local_editor_buffers_and_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local.txt");
        fs::write(&path, SAVED_BYTES).unwrap();
        let mut app = test_app();
        app.workbench = Workbench::open_local_source(
            WorkbenchStyles::default(),
            &path,
            None,
            &(),
            |_, _, _| Ok(()),
        )
        .unwrap();
        assert!(app.workbench.paste(UNSAVED));
        assert!(app.workbench.blocks_workspace_change());
        assert_eq!(app.sandbox_action_blocker(false), Some(UNSAVED_DRAFT_ERR));
        assert!(app.workbench.blocks_workspace_change());
        assert_eq!(fs::read_to_string(&path).unwrap(), SAVED_BYTES);
        assert!(app.sandbox_live.transfer.is_none());
    }

    #[test_case(false; "idle_local")]
    #[test_case(true; "streaming_agent")]
    fn sandbox_actions_gate_active_agents_and_unsaved_local_input(streaming: bool) {
        let mut app = test_app();
        if streaming {
            app.status = Status::Streaming;
        }
        assert_eq!(app.sandbox_action_blocker(true).is_some(), streaming);
        app.input_box.set_input(UNSAVED.into());
        assert!(app.sandbox_action_blocker(true).is_some());
        assert_eq!(app.active_input_text(), UNSAVED);
        assert!(app.workspace_session.is_none());
    }

    #[test_case(false; "main_composer")]
    #[test_case(true; "subagent_composer")]
    fn sandbox_transition_blocks_image_only_drafts(subagent: bool) {
        let mut app = test_app();
        let input = if subagent {
            &mut app.subagent_input_box
        } else {
            &mut app.input_box
        };
        input.attach_image(ImageSource::new(ImageMediaType::Png, Arc::from(IMAGE_DATA)));

        assert_eq!(app.sandbox_action_blocker(true), Some(UNSAVED_DRAFT_ERR));
        assert_eq!(app.sandbox_action_blocker(false), None);
    }

    #[test_case(0; "startup")]
    #[test_case(3; "idle_ticks")]
    fn embedded_startup_does_not_allocate_or_load_sandbox(ticks: usize) {
        let mut app = test_app();
        for _ in 0..ticks {
            assert_eq!(app.poll_sandbox(), Dirty::NO);
        }
        assert!(!app.sandbox_manager.allocated());
        assert!(app.sandbox_reply.is_none());
        assert!(app.sandbox_snapshot_request().is_none());
        assert!(!app.sandbox_manager.is_open());
    }

    #[test_case("/sandbox start"; "no_lifecycle_command")]
    #[test_case("/sandbox delete"; "no_instance_delete")]
    fn unsupported_commands_do_not_load_or_allocate(command: &str) {
        let mut app = test_app();
        assert!(app.run_cmdline(command, 0).unwrap().is_empty());
        assert!(!app.sandbox_manager.allocated());
        assert!(app.sandbox_reply.is_none());
    }

    #[test_case(key::UNDO.to_key_event(); "undo_never_reaches_composer")]
    #[test_case(key::SAVE.to_key_event(); "save_never_reaches_composer")]
    fn sandbox_owns_editor_keys(event: KeyEvent) {
        let mut app = test_app();
        let _ = app
            .sandbox_manager
            .open(app.state.session.id, SandboxView::Profiles);
        assert!(app.update(Msg::Key(event)).is_empty());
        assert!(app.sandbox_manager.is_open());
        assert!(app.active_input_text().is_empty());
    }
}
