use super::App;
use crate::components::Overlay;
use crate::components::sandbox_manager::{SandboxAction, SandboxView};
use crate::repaint::Dirty;
use crate::sandbox::transfer::{TransferCommand, TransferReply, TransferWorker};
use crate::sandbox::{LiveRequest, start_live, start_snapshot};
use crate::sandbox::{SandboxSnapshot, SandboxSnapshotRequest, start_store_effect};
use caudra_agent::AgentEvent;
use flume::TryRecvError;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const UNSAVED_DRAFT_ERR: &str =
    "Save or discard unsaved local editor/composer drafts before changing sandbox authority";

impl App {
    pub(super) fn open_sandbox(&mut self, args: &str) {
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
        match action {
            SandboxAction::None => {}
            SandboxAction::Copy(text) => self.copy_to_clipboard(&text),
            SandboxAction::Store { ticket, effect } => {
                self.sandbox_reply = Some(start_store_effect(ticket, effect));
            }
            SandboxAction::Live(request) => self.sandbox_live.queued = Some(request),
            SandboxAction::Transfer(command) => self.sandbox_live.transfer_queued = Some(command),
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

    pub(crate) fn transfer_failed(&mut self, message: String) {
        if let Some(scope) = self.sandbox_snapshot_request() {
            self.sandbox_manager
                .receive_transfer(&scope, TransferReply::Failed(message));
        }
    }

    pub(crate) fn start_transfer(&mut self, command: TransferCommand) {
        let result = match command {
            TransferCommand::Open { scope, link } => {
                if self.sandbox_snapshot_request().as_ref() != Some(&scope)
                    || !self.sandbox_manager.is_open()
                {
                    return;
                }
                self.sandbox_live.transfer = None;
                match self.sandbox_live.transfer_connector.clone() {
                    Some(connector) => TransferWorker::start(scope, link, connector, &self.storage)
                        .map(|worker| self.sandbox_live.transfer = Some(worker)),
                    _ => Err("Transfer connector unavailable; no fallback".into()),
                }
            }
            TransferCommand::Close => {
                if let Some(worker) = self.sandbox_live.transfer.as_mut() {
                    worker.cancel();
                }
                Ok(())
            }
            command => self
                .sandbox_live
                .transfer
                .as_ref()
                .ok_or_else(|| "Reconnect Compare before requesting another operation".into())
                .and_then(|worker| worker.send(command)),
        };
        if let Err(error) = result {
            self.transfer_failed(error);
        }
    }

    fn poll_transfer(&mut self) -> Dirty {
        let current = self.sandbox_snapshot_request();
        let Some(worker) = self.sandbox_live.transfer.as_mut() else {
            return Dirty::NO;
        };
        if current.as_ref() != Some(&worker.scope) || !self.sandbox_manager.is_open() {
            worker.cancel();
        }
        let mut dirty = Dirty::NO;
        let mut closed = false;
        for reply in worker.replies.try_iter() {
            closed |= matches!(reply, TransferReply::Closed);
            self.sandbox_manager.receive_transfer(&worker.scope, reply);
            dirty = Dirty::YES;
        }
        closed |= worker.replies.is_disconnected();
        for envelope in worker.permission_events.try_iter() {
            match envelope.event {
                AgentEvent::PermissionRequest(request) => {
                    if worker.permissions.pending_request(&request.id).is_some() {
                        self.permission_prompt
                            .enqueue(request, Some("workspace transfer".into()));
                    }
                }
                AgentEvent::PermissionRequestUpdated(request) => {
                    self.permission_prompt.update(request);
                }
                AgentEvent::PermissionRequestResolved { request_id, .. } => {
                    self.permission_prompt.resolve_pending(&request_id);
                }
                _ => continue,
            }
            dirty = Dirty::YES;
        }
        for event in worker.progress.try_iter() {
            self.sandbox_manager.transfer_progress(&worker.scope, event);
            dirty = Dirty::YES;
        }
        if closed {
            self.sandbox_live.transfer = None;
        }
        dirty
    }

    pub(crate) fn sandbox_action_blocker(&self, transition: bool) -> Option<&'static str> {
        if self.sandbox_live.transfer.is_some() || self.sandbox_live.reply.is_some() {
            return Some(
                "Wait for sandbox operations and close the transfer connection before changing sandbox authority",
            );
        }
        if self.has_lifecycle_work() || self.awaiting_input() || self.permission_mutation_pending()
        {
            return Some(
                "Wait for the active agent, permission or restore operation before a sandbox mutation",
            );
        }
        if self.workbench.is_busy() {
            return Some(
                "Wait for pending Workbench reads or writes before changing sandbox authority",
            );
        }
        if self.workbench_blocks_workspace_change()
            || (transition
                && (!self.input_box.is_empty()
                    || !self.subagent_input_box.is_empty()
                    || !self.subagent_drafts.is_empty()))
        {
            return Some(UNSAVED_DRAFT_ERR);
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
                    self.sandbox_manager.live_failed("Worker disconnected: outcome unknown. Draft retained. Reconcile; never replay Create.".into());
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
        let dirty = self.poll_transfer() | self.poll_sandbox_live();
        let Some(receiver) = &self.sandbox_reply else {
            return dirty;
        };
        match receiver.try_recv() {
            Ok(reply) => {
                self.sandbox_reply = None;
                let action = self.sandbox_manager.receive(reply);
                self.handle_sandbox_action(action);
                Dirty::YES
            }
            Err(TryRecvError::Empty) => dirty,
            Err(TryRecvError::Disconnected) => {
                self.sandbox_reply = None;
                self.sandbox_manager.disconnected();
                Dirty::YES
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{remote_workspace_session, test_app};
    use super::UNSAVED_DRAFT_ERR;
    use crate::app::Msg;
    use crate::components::Overlay;
    use crate::components::Status;
    use crate::components::keybindings::key;
    use crate::components::permission_prompt::PermissionDecision;
    use crate::components::sandbox_manager::{SandboxAction, SandboxView};
    use crate::repaint::Dirty;
    use crate::sandbox::transfer::{TransferCommand, TransferLink, transfer_permissions};
    use crate::sandbox::{StoreReply, execute_store_effect};
    use caudra_agent::permissions::{PermissionAnswer, PermissionManager, PluginRuleStore};
    use caudra_agent::workspace_transfer::{
        LocalAccess, LocalRootIdentity, RemoteRootIdentity, TransferAuthorization, TransferRoots,
    };
    use caudra_agent::{AgentEvent, CancelToken, EventSender};
    use caudra_config::{
        PermissionsConfig,
        sandbox::{SandboxName, persistence::SandboxStore},
    };
    use caudra_providers::{ImageMediaType, ImageSource};
    use caudra_workbench::{Workbench, WorkbenchStyles};
    use caudra_workcell::NativeTransferAuthorization;
    use caudra_workspace::WorkspacePath;
    use crossterm::event::KeyEvent;
    use futures_lite::future;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use test_case::test_case;

    const UNSAVED: &str = "unsent local draft";
    const IMAGE_DATA: &str = "dGVzdA==";
    const SAVED_BYTES: &str = "saved local file\n";
    const PERMISSIONS: &str = ".caudra/permissions.toml";
    const DENY_TRANSFER: &str = "[workspace_transfer]\ndeny = true\n";
    const DENY_DEFAULT: &str = "default = 'deny'\n";
    const TRANSFER_TEST_DONE: &str = "test authorization complete";
    const PERMISSION_TIMEOUT: Duration = Duration::from_secs(10);
    const DOTENV_KEY: &str = "CAUDRA_TRANSFER_TEST_NO_DOTENV";
    const PRIVATE_MODE: u32 = 0o700;
    static TRANSFER_TEST_LOCK: Mutex<()> = Mutex::new(());

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
        let scope = app.sandbox_snapshot_request().unwrap();
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
            link: TransferLink {
                name: SandboxName::parse("test").unwrap(),
                instance_revision: scope.configuration_revision.clone(),
                configuration_revision: scope.configuration_revision.clone(),
                local_root: root_b.path().into(),
                remote_root: WorkspacePath::root(),
            },
            scope,
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
        app.sandbox_live.transfer = None;
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

    #[test_case(key::UNDO.to_key_event(); "undo_is_not_suspend")]
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
