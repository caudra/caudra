use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use caudra_agent::AgentEvent;
use caudra_agent::workspace_transfer::{
    ComparisonKind, FileOutcome, FilePreview, JournalEntry, NodeKind, PlanReview,
    TransferAction as Direction, TransferEvent, TransferRoots as EngineRoots,
};
use caudra_config::sandbox::persistence::SandboxStore;
use caudra_sandbox::Controller;
use caudra_workbench::{
    WorkbenchAction,
    transfer::{
        TransferAction, TransferAvailability, TransferDirection, TransferEffect, TransferEntry,
        TransferNodeKind, TransferOutcome, TransferPreview, TransferProgress, TransferReview,
        TransferReviewEntry, TransferRoots, TransferSnapshot, TransferStatus,
    },
};
use caudra_workspace::{OperationId, TransferDigest, WorkspacePath};
use crossterm::event::{KeyEvent, KeyEventKind};

use super::{Action, App, Msg, sandbox::attached_sandbox_instance};
use crate::components::{
    keybindings::{self, key},
    permission_prompt::PromptMouse,
};
use crate::repaint::Dirty;
use crate::sandbox::{
    InitialSeed,
    transfer::{
        ComparisonView, TransferCommand, TransferLink, TransferReply, TransferScope, TransferWorker,
    },
};

const STALE: &str = "Attachment or transfer roots changed; compare again";
const CONNECT: &str = "Compare explicit roots before requesting another operation";
const NO_ROLLBACK: &str = "No rollback or whole-tree atomicity. Unknown outcomes are queried, never replayed. Metadata: content and executable bit only.";

impl App {
    pub(super) fn workbench_leader(&mut self, event: KeyEvent) -> WorkbenchAction {
        if self.workbench.transfer_input_active() && keybindings::leader::WORKBENCH.matches(event) {
            self.workbench.close_transfer()
        } else {
            self.workbench.handle_leader(event)
        }
    }
    pub(super) fn sync_transfer_availability(&mut self) {
        let availability = attached_sandbox_instance(&self.state.session, &self.sandbox_live)
            .and_then(|label| {
                let binding = self.state.session.workspace_binding()?;
                Some(TransferAvailability {
                    attachment: format!(
                        "{}:{}",
                        self.state.session.id,
                        serde_json::to_string(binding).ok()?
                    ),
                    label: label.to_owned(),
                    local_root: None,
                    remote_root: WorkspacePath::root().to_string(),
                    directory_effects: self
                        .sandbox_live
                        .transfer
                        .as_ref()
                        .is_some_and(|worker| worker.directory_effects),
                })
            });
        if self.sandbox_live.transfer.is_none() || availability.is_some() {
            self.bound_workbench_mut()
                .set_transfer_availability(availability);
        }
    }

    pub(crate) fn install_initial_seed(&mut self, seed: Option<InitialSeed>) {
        self.sync_transfer_availability();
        let Some(seed) = seed else {
            return;
        };
        let generation = self.bound_workbench().transfer_generation();
        match self.transfer_scope(generation) {
            Ok(scope) if scope.name == seed.name && scope.instance_revision == seed.revision => {
                self.bound_workbench_mut().show_transfer(
                    TransferRoots {
                        local: seed.local.to_string_lossy().into_owned(),
                        remote: seed.remote.to_string(),
                    },
                    TransferDirection::Seed,
                );
            }
            _ => self.flash("Initial seed offer expired; nothing was uploaded".into()),
        }
    }

    fn transfer_scope(&self, generation: u64) -> Result<TransferScope, String> {
        attached_sandbox_instance(&self.state.session, &self.sandbox_live)
            .ok_or("Attach an authenticated sandbox before transferring")?;
        let binding = self.state.session.workspace_binding().ok_or(STALE)?.clone();
        let controller = Controller::new(&self.storage).map_err(|error| error.to_string())?;
        let record = if let Some(name) = &self.sandbox_live.name {
            controller
                .store()
                .get(name)
                .map_err(|error| error.to_string())?
        } else {
            controller
                .snapshots()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|record| Some(record.id) == binding.sandbox_record())
                .ok_or(STALE)?
        };
        if Some(record.id) != binding.sandbox_record() || record.detached {
            return Err(STALE.into());
        }
        let configuration = SandboxStore::user_global()
            .and_then(|store| store.load())
            .map_err(|error| error.to_string())?;
        Ok(TransferScope {
            conversation: self.state.session.id,
            binding,
            name: record.name.clone(),
            instance_revision: record.revision().map_err(|error| error.to_string())?,
            configuration_revision: configuration.saved().revision().clone(),
            generation,
        })
    }

    pub(crate) fn transfer_scope_current(&self, scope: &TransferScope) -> bool {
        scope.conversation == self.state.session.id
            && self.state.session.workspace_binding() == Some(&scope.binding)
            && self
                .sandbox_live
                .name
                .as_ref()
                .is_none_or(|name| name == &scope.name)
            && attached_sandbox_instance(&self.state.session, &self.sandbox_live).is_some()
            && self.bound_workbench().transfer_generation() == scope.generation
    }

    pub(crate) fn handle_transfer_action(&mut self, action: TransferAction) {
        let result = self.queue_transfer_action(action);
        if let Err(error) = result {
            self.transfer_failed(error);
        }
    }

    fn queue_transfer_action(&mut self, action: TransferAction) -> Result<(), String> {
        if let TransferAction::Cancel { generation } = action {
            if generation != self.bound_workbench().transfer_generation() {
                return Err(STALE.into());
            }
            self.sandbox_live.transfer_queued = None;
            if let Some(worker) = self.sandbox_live.transfer.as_mut() {
                worker.cancel();
                self.bound_workbench_mut()
                    .set_transfer_connection(generation, true, true);
            } else {
                self.bound_workbench_mut()
                    .set_transfer_connection(generation, false, false);
            }
            return Ok(());
        }
        let (scope, command) = match action {
            TransferAction::Compare { generation, roots } => {
                if generation != self.bound_workbench().transfer_generation() {
                    return Err(STALE.into());
                }
                let scope = self.transfer_scope(generation)?;
                let link = TransferLink {
                    name: scope.name.clone(),
                    instance_revision: scope.instance_revision.clone(),
                    configuration_revision: scope.configuration_revision.clone(),
                    local_root: PathBuf::from(roots.local),
                    remote_root: WorkspacePath::new(roots.remote)
                        .map_err(|error| error.to_string())?,
                    attached_binding: Some(scope.binding.clone()),
                };
                if let Some(worker) = self.sandbox_live.transfer.as_mut() {
                    worker.cancel();
                }
                (
                    scope.clone(),
                    TransferCommand::Open {
                        scope: Box::new(scope),
                        link: Box::new(link),
                    },
                )
            }
            action => {
                let scope = self
                    .sandbox_live
                    .transfer
                    .as_ref()
                    .ok_or(CONNECT)?
                    .scope
                    .clone();
                let (generation, command) = match action {
                    TransferAction::Inspect { generation, path } => (
                        generation,
                        TransferCommand::Inspect(
                            WorkspacePath::new(path).map_err(|error| error.to_string())?,
                        ),
                    ),
                    TransferAction::Review {
                        generation,
                        direction,
                        paths,
                    } => (
                        generation,
                        TransferCommand::Review(
                            engine_direction(direction),
                            paths
                                .into_iter()
                                .map(WorkspacePath::new)
                                .collect::<Result<Vec<_>, _>>()
                                .map_err(|error| error.to_string())?,
                        ),
                    ),
                    TransferAction::Execute { generation, digest } => (
                        generation,
                        TransferCommand::Execute(
                            TransferDigest::new(digest).map_err(|error| error.to_string())?,
                        ),
                    ),
                    TransferAction::Reconcile { generation } => {
                        (generation, TransferCommand::Reconcile)
                    }
                    TransferAction::Cancel { .. } | TransferAction::Compare { .. } => {
                        return Err(STALE.into());
                    }
                };
                if generation != scope.generation || !self.transfer_scope_current(&scope) {
                    return Err(STALE.into());
                }
                (scope, command)
            }
        };
        if self.sandbox_live.transfer_queued.is_some() {
            return Err("Transfer request already queued".into());
        }
        self.sandbox_live.transfer_queued = Some((scope, command));
        Ok(())
    }

    pub(crate) fn transfer_failed(&mut self, message: String) {
        let generation = self.bound_workbench().transfer_generation();
        let recovery_required = self
            .sandbox_live
            .transfer
            .as_ref()
            .is_some_and(|worker| worker.recovery_required);
        self.bound_workbench_mut().receive_transfer_outcome(
            generation,
            TransferOutcome {
                entries: vec![message],
                recovery_required,
            },
        );
        if self.sandbox_live.transfer.is_none() {
            self.bound_workbench_mut()
                .set_transfer_connection(generation, false, false);
        }
    }

    pub(crate) fn start_transfer(&mut self, command: TransferCommand) {
        let result = match command {
            TransferCommand::Open { scope, link } => {
                if !self.transfer_scope_current(&scope) || self.sandbox_live.transfer.is_some() {
                    return;
                }
                let generation = scope.generation;
                match self.sandbox_live.transfer_connector.clone() {
                    Some(connector) => {
                        TransferWorker::start(*scope, *link, connector, &self.storage).map(
                            |worker| {
                                self.sandbox_live.transfer = Some(worker);
                                self.bound_workbench_mut()
                                    .set_transfer_connection(generation, true, false);
                            },
                        )
                    }
                    None => Err("Transfer connector unavailable; no fallback".into()),
                }
            }
            command => self
                .sandbox_live
                .transfer
                .as_ref()
                .ok_or_else(|| CONNECT.to_owned())
                .and_then(|worker| worker.send(command)),
        };
        if let Err(error) = result {
            self.transfer_failed(error);
        }
    }

    pub(super) fn poll_transfer(&mut self) -> Dirty {
        let previous_generation = self.bound_workbench().transfer_generation();
        self.sync_transfer_availability();
        let current = self
            .sandbox_live
            .transfer
            .as_ref()
            .is_some_and(|worker| self.transfer_scope_current(&worker.scope));
        let visible =
            self.bound_workbench().is_open() && self.bound_workbench().transfer_input_active();
        let Some(worker) = self.sandbox_live.transfer.as_mut() else {
            return Dirty::from(
                self.bound_workbench().is_open()
                    && previous_generation != self.bound_workbench().transfer_generation(),
            );
        };
        if !current || !visible {
            worker.cancel();
        }
        let generation = worker.scope.generation;
        let settled = worker.settled();
        let replies: Vec<_> = worker.replies.try_iter().collect();
        let progress: Vec<_> = worker.progress.try_iter().collect();
        let mut dirty = Dirty::from(!replies.is_empty() || !progress.is_empty());
        for envelope in worker.permission_events.try_iter() {
            match envelope.event {
                AgentEvent::PermissionRequest(request)
                    if current && worker.permissions.pending_request(&request.id).is_some() =>
                {
                    self.permission_prompt
                        .enqueue(request, Some("workspace transfer".into()));
                }
                AgentEvent::PermissionRequestUpdated(request) if current => {
                    self.permission_prompt.update(request);
                }
                AgentEvent::PermissionRequestResolved { request_id, .. } => {
                    self.permission_prompt.resolve_pending(&request_id);
                }
                _ => continue,
            }
            dirty = Dirty::YES;
        }
        for reply in replies {
            if !current {
                continue;
            }
            match reply {
                TransferReply::Compared(comparison, recovery, directory_effects) => {
                    if let Some(worker) = self.sandbox_live.transfer.as_mut() {
                        worker.directory_effects = directory_effects;
                    }
                    self.sync_transfer_availability();
                    self.bound_workbench_mut()
                        .receive_transfer_snapshot(generation, snapshot(&comparison));
                    let (entries, required) = recovery_summary(recovery);
                    if let Some(worker) = self.sandbox_live.transfer.as_mut() {
                        worker.recovery_required = required;
                    }
                    self.bound_workbench_mut()
                        .receive_transfer_recovery(generation, entries, required);
                }
                TransferReply::Reviewed(plan) => {
                    self.bound_workbench_mut()
                        .receive_transfer_review(generation, review(&plan.digest, &plan.review));
                }
                TransferReply::Inspected(preview) => {
                    let (local, local_truncated) = preview_text(preview.local_preview.as_ref());
                    let (remote, remote_truncated) = preview_text(preview.remote_preview.as_ref());
                    self.bound_workbench_mut().receive_transfer_preview(
                        generation,
                        TransferPreview {
                            path: preview.path.to_string(),
                            local,
                            remote,
                            truncated: local_truncated || remote_truncated,
                            summary: format!(
                                "Local {}: {:?}\nSandbox {}: {:?}",
                                preview_kind(preview.local_preview.as_ref()),
                                preview.local.as_ref().map(|file| &file.content),
                                preview_kind(preview.remote_preview.as_ref()),
                                preview.remote.as_ref().map(|file| &file.content)
                            ),
                        },
                    );
                }
                TransferReply::Finished(report) => {
                    let (mut entries, mut recovery_required) =
                        recovery_summary(report.recovery.clone());
                    entries.insert(
                        0,
                        format!(
                            "Result {} · {}",
                            report.result_id,
                            report.stopped.as_deref().unwrap_or("settled")
                        ),
                    );
                    match serde_json::from_value::<BTreeMap<OperationId, FileOutcome>>(
                        report.outcomes.clone(),
                    ) {
                        Ok(outcomes) => {
                            for (id, outcome) in outcomes {
                                recovery_required |= outcome == FileOutcome::Unknown;
                                entries.push(format!("{}: {outcome:?}", id.as_str()));
                            }
                        }
                        Err(error) => {
                            recovery_required = true;
                            entries.push(format!("Outcomes unavailable: {error}; never replay"));
                        }
                    }
                    recovery_required |= report
                        .cleanup_deferred
                        .as_array()
                        .is_none_or(|entries| !entries.is_empty());
                    if let Some(worker) = self.sandbox_live.transfer.as_mut() {
                        worker.recovery_required = recovery_required;
                    }
                    self.bound_workbench_mut().receive_transfer_outcome(
                        generation,
                        TransferOutcome {
                            entries,
                            recovery_required,
                        },
                    );
                    self.bound_workbench_mut().refresh_after_transfer();
                }
                TransferReply::Failed(message) => self.transfer_failed(message),
                TransferReply::Closed => {}
            }
        }
        if current {
            for event in progress {
                let Some(worker) = self.sandbox_live.transfer.as_mut() else {
                    break;
                };
                match &event {
                    TransferEvent::Planned { files, .. } => {
                        worker.total = *files;
                        worker.completed = 0;
                    }
                    TransferEvent::Settled { .. } => worker.completed += 1,
                    _ => {}
                }
                let progress = TransferProgress {
                    message: format!("{event:?}"),
                    completed: worker.completed,
                    total: worker.total,
                };
                self.bound_workbench_mut()
                    .receive_transfer_progress(generation, progress);
            }
        }
        if settled {
            self.sandbox_live.transfer = None;
            let current_generation = self.bound_workbench().transfer_generation();
            self.bound_workbench_mut()
                .set_transfer_connection(current_generation, false, false);
            self.sync_transfer_availability();
            dirty = Dirty::YES;
        }
        dirty
    }

    pub(super) fn transfer_input(&mut self, msg: Msg) -> Vec<Action> {
        let owned_prompt = self.permission_prompt.request_id().is_some_and(|id| {
            self.sandbox_live
                .transfer
                .as_ref()
                .is_some_and(|worker| worker.permissions.pending_request(id).is_some())
        });
        if owned_prompt {
            match msg {
                Msg::Key(key) => {
                    if let Some(decision) = self.permission_prompt.handle_key(key) {
                        self.apply_permission_decision(decision);
                    }
                }
                Msg::Mouse(event) => {
                    if let PromptMouse::Decided(decision) =
                        self.permission_prompt.handle_mouse(event)
                    {
                        self.apply_permission_decision(decision);
                    }
                }
                Msg::Paste(text) => {
                    self.permission_prompt.handle_paste(&text);
                }
                Msg::Scroll { delta, .. } => self.permission_prompt.scroll(delta),
                Msg::Agent(_) => {}
            }
            return Vec::new();
        }
        if self.parked_workbench.is_some()
            || !self.workbench.is_open()
            || !self.workbench.transfer_input_active()
            || self.sandbox_live.transfer.is_none()
        {
            return Vec::new();
        }
        let action = match msg {
            Msg::Key(event) if event.kind == KeyEventKind::Release => WorkbenchAction::Consumed,
            Msg::Key(event) if self.which_key.is_armed() => {
                self.which_key.disarm();
                self.workbench_leader(keybindings::normalize_leader_key(event))
            }
            Msg::Key(event) if key::LEADER.matches(event) => {
                self.which_key.arm();
                WorkbenchAction::Consumed
            }
            Msg::Key(event) => self.workbench.handle_key(event),
            Msg::Mouse(event) => self.workbench.handle_mouse(event),
            Msg::Paste(text) => {
                self.workbench.paste(&text);
                WorkbenchAction::Consumed
            }
            Msg::Scroll { column, row, delta } => {
                self.workbench.scroll(column, row, delta as isize);
                WorkbenchAction::Consumed
            }
            Msg::Agent(_) => WorkbenchAction::Consumed,
        };
        self.handle_workbench_action(action)
    }
}

fn engine_direction(direction: TransferDirection) -> Direction {
    match direction {
        TransferDirection::Push => Direction::Push,
        TransferDirection::Pull => Direction::Pull,
        TransferDirection::Seed => Direction::Seed,
    }
}

fn roots(roots: &EngineRoots) -> TransferRoots {
    TransferRoots {
        local: roots.local.canonical_path().to_string_lossy().into_owned(),
        remote: roots.remote.cwd.to_string(),
    }
}

fn node(kind: Option<&NodeKind>) -> Option<TransferNodeKind> {
    match kind {
        Some(NodeKind::File) => Some(TransferNodeKind::File),
        Some(NodeKind::Directory) => Some(TransferNodeKind::Directory),
        _ => None,
    }
}

fn snapshot(comparison: &ComparisonView) -> TransferSnapshot {
    TransferSnapshot {
        roots: roots(&comparison.context.roots),
        entries: comparison
            .rows
            .iter()
            .map(|row| TransferEntry {
                path: row.path.to_string(),
                local: node(row.local_kind.as_ref()),
                remote: node(row.remote_kind.as_ref()),
                status: match row.kind {
                    ComparisonKind::Equal => TransferStatus::Equal,
                    ComparisonKind::LocalOnly => TransferStatus::LocalOnly,
                    ComparisonKind::RemoteOnly => TransferStatus::RemoteOnly,
                    ComparisonKind::Conflict if row.local_kind != row.remote_kind => {
                        TransferStatus::TypeConflict
                    }
                    ComparisonKind::Conflict => TransferStatus::Different,
                    ComparisonKind::Excluded => TransferStatus::Excluded,
                    ComparisonKind::Unsupported => TransferStatus::Unsupported,
                    ComparisonKind::Incomplete => TransferStatus::Incomplete,
                },
                bytes: row
                    .local
                    .as_ref()
                    .or(row.remote.as_ref())
                    .map_or(0, |file| file.content.size_bytes),
            })
            .collect(),
        complete: comparison.complete,
        notice: Some(format!(
            "Content comparison; complete={}. {NO_ROLLBACK}",
            comparison.complete
        )),
    }
}

fn review(digest: &TransferDigest, review: &PlanReview) -> TransferReview {
    let pull = review.action == Direction::Pull;
    let mut directories = BTreeSet::new();
    let mut entries = Vec::new();
    for file in &review.files {
        directories.extend(file.create_directories.iter().map(ToString::to_string));
        entries.push(TransferReviewEntry {
            path: file.path.to_string(),
            effect: if (if pull { &file.local } else { &file.remote }).is_some() {
                TransferEffect::Overwrite
            } else {
                TransferEffect::New
            },
            bytes: (if pull { &file.remote } else { &file.local })
                .as_ref()
                .map_or(0, |stamp| stamp.content.size_bytes),
        });
    }
    for directory in &review.directories {
        directories.insert(directory.path.to_string());
        directories.extend(directory.create_directories.iter().map(ToString::to_string));
    }
    entries.extend(directories.into_iter().map(|path| TransferReviewEntry {
        path,
        effect: TransferEffect::Mkdir,
        bytes: 0,
    }));
    TransferReview {
        digest: digest.as_str().to_owned(),
        roots: roots(&review.context.roots),
        direction: match review.action {
            Direction::Push => TransferDirection::Push,
            Direction::Pull => TransferDirection::Pull,
            Direction::Seed => TransferDirection::Seed,
        },
        entries,
        skipped: review.skipped.iter().map(ToString::to_string).collect(),
        executable: true,
        notice: Some(NO_ROLLBACK.into()),
    }
}

fn preview_text(preview: Option<&FilePreview>) -> (Option<String>, bool) {
    match preview {
        Some(FilePreview::TextPrefix { text, truncated }) => (Some(text.clone()), *truncated),
        _ => (None, false),
    }
}

fn preview_kind(preview: Option<&FilePreview>) -> &'static str {
    match preview {
        Some(FilePreview::TextPrefix { .. }) => "bounded text",
        Some(FilePreview::BinarySummary { .. }) => "binary",
        None => "absent",
    }
}

fn recovery_summary(recovery: serde_json::Value) -> (Vec<String>, bool) {
    match serde_json::from_value::<Vec<JournalEntry>>(recovery) {
        Ok(records) => {
            let required = records
                .iter()
                .any(|entry| entry.state.blocks() || entry.cleanup_pending);
            let entries = records
                .iter()
                .filter(|entry| entry.state.blocks() || entry.cleanup_pending)
                .map(|entry| {
                    format!(
                        "Recovery {} {}: {:?}; cleanup pending={}; query only, never replay",
                        entry.operation_id.as_str(),
                        entry.path,
                        entry.state,
                        entry.cleanup_pending
                    )
                })
                .collect();
            (entries, required)
        }
        Err(error) => (
            vec![format!(
                "Recovery records unavailable: {error}; query only, never replay"
            )],
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{App, TransferCommand, TransferLink, TransferScope, recovery_summary};
    use crate::agent::shared_queue::{QueueItem, queue};
    use crate::{
        AppSession,
        app::{
            Msg,
            tests::{remote_workspace_session, test_app},
        },
        components::{Overlay, keybindings::key},
    };
    use caudra_config::sandbox::{Revision, SandboxName};
    use caudra_storage::{id::CaudraId, workspace_binding::StoredWorkspaceBinding};
    use caudra_workbench::{
        DocumentKey, TabLabel, WorkbenchAction,
        transfer::{TransferDirection, TransferRoots},
    };
    use caudra_workspace::WorkspacePath;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;
    use std::{fs, sync::Arc, time::Duration};
    use test_case::test_case;

    const REVISION: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SAVED: &str = "saved file\n";
    const CHANGED: &str = "must not be applied";
    const STOPPED: &str = "test worker stopped";
    const TIMEOUT: Duration = Duration::from_secs(10);

    fn attached() -> App {
        let mut app = test_app();
        let workspace = remote_workspace_session();
        let binding = StoredWorkspaceBinding::new_with_cursor(
            workspace.binding().clone(),
            workspace.cursor().clone(),
            None,
        )
        .unwrap()
        .with_sandbox_record(CaudraId::generate())
        .unwrap();
        app.state.session = Arc::new(AppSession::new_with_workspace("test", ".", binding));
        app.workspace_session = Some(workspace);
        app.sandbox_live.name = Some(SandboxName::parse("attached").unwrap());
        app.sandbox_live.readiness = Some(Arc::new(|| true));
        app.sync_transfer_availability();
        app
    }

    fn scope(app: &App) -> TransferScope {
        TransferScope {
            conversation: app.state.session.id,
            binding: app.state.session.workspace_binding().unwrap().clone(),
            name: app.sandbox_live.name.clone().unwrap(),
            instance_revision: Revision::parse(REVISION).unwrap(),
            configuration_revision: Revision::parse(REVISION).unwrap(),
            generation: app.workbench.transfer_generation(),
        }
    }

    #[test_case(0; "conversation")]
    #[test_case(1; "attached_name")]
    #[test_case(2; "root_generation")]
    #[test_case(3; "authenticated_binding")]
    #[test_case(4; "disconnected")]
    fn stale_scope_is_not_current(change: usize) {
        let mut app = attached();
        let mut scope = scope(&app);
        assert!(app.transfer_scope_current(&scope));
        match change {
            0 => scope.conversation = CaudraId::generate(),
            1 => scope.name = SandboxName::parse("other").unwrap(),
            2 => scope.generation += 1,
            3 => scope.binding = StoredWorkspaceBinding::local_from_cwd("/tmp"),
            _ => app.sandbox_live.readiness = Some(Arc::new(|| false)),
        }
        assert!(!app.transfer_scope_current(&scope));
    }

    #[test_case(false; "local")]
    #[test_case(true; "attached")]
    fn transfer_availability_does_not_open_manager_or_query_instance(bound: bool) {
        let mut app = if bound { attached() } else { test_app() };
        app.sync_transfer_availability();
        assert_eq!(app.workbench.open_transfer(), bound);
        assert!(!app.sandbox_manager.is_open());
        assert!(app.sandbox_live.snapshot.is_none());
        assert!(app.sandbox_live.scope.is_none());
        assert!(app.sandbox_live.transfer.is_none());
    }

    #[test]
    fn queued_compare_pending_does_not_block_its_own_admission() {
        let root = tempfile::tempdir().unwrap();
        let mut app = attached();
        assert!(app.workbench.show_transfer(
            TransferRoots {
                local: root.path().to_string_lossy().into_owned(),
                remote: String::new()
            },
            TransferDirection::Push
        ));
        let action = app
            .workbench
            .handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
        assert!(matches!(action, WorkbenchAction::Transfer(_)));
        assert!(app.workbench.is_busy());
        assert!(app.sandbox_action_blocker(false).is_some());
        assert_eq!(app.transfer_start_blocker(), None);
        let mut other = test_app();
        assert_eq!(other.sandbox_action_blocker(false), None);
        other.workbench.open(root.path());
        let key = DocumentKey("unsaved".into());
        other.workbench.open_document(
            key.clone(),
            TabLabel {
                title: "draft".into(),
                status: "draft".into(),
            },
            SAVED,
        );
        other.workbench.paste(CHANGED);
        assert!(other.workbench.has_unsaved_document(&key));
        assert!(other.sandbox_action_blocker(false).is_some());
        assert!(other.transfer_start_blocker().is_some());
    }

    #[test_case(false; "escape")]
    #[test_case(true; "close_chord")]
    fn lease_isolates_editor_composer_and_other_sessions_until_cleanup(close_chord: bool) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        fs::write(&path, SAVED).unwrap();
        let mut app = attached();
        app.workbench.open_at(root.path(), &path, None);
        app.workbench.paste(CHANGED);
        assert!(app.workbench.open_transfer());
        let scope = scope(&app);
        let (started, ready) = flume::bounded(1);
        let (released, release) = flume::bounded(1);
        app.sandbox_live.transfer_connector = Some(Arc::new(move |_, host| {
            started.send(()).unwrap();
            smol::block_on(host.cancel.cancelled());
            release.recv().unwrap();
            Err(STOPPED.into())
        }));
        app.start_transfer(TransferCommand::Open {
            link: Box::new(TransferLink {
                name: scope.name.clone(),
                instance_revision: scope.instance_revision.clone(),
                configuration_revision: scope.configuration_revision.clone(),
                local_root: root.path().into(),
                remote_root: WorkspacePath::root(),
                attached_binding: Some(scope.binding.clone()),
            }),
            scope: Box::new(scope),
        });
        ready.recv_timeout(TIMEOUT).unwrap();
        let (queued, dispatch) = queue();
        queued.push(QueueItem::Compact { run_id: 1 });
        assert!(dispatch.claim_idle(0).is_empty());
        assert_eq!(queued.len(), 1);
        let mut other = test_app();
        other.workbench.open_at(root.path(), &path, None);
        other.update(Msg::Paste(CHANGED.into()));
        other.update(Msg::Key(key::SAVE.to_key_event()));
        app.update(Msg::Key(key::SAVE.to_key_event()));
        assert_eq!(fs::read_to_string(&path).unwrap(), SAVED);
        assert!(other.active_input_text().is_empty());
        if close_chord {
            app.update(Msg::Key(key::LEADER.to_key_event()));
            app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::NONE,
            )));
        } else {
            app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        }
        assert!(crate::sandbox::transfer::active());
        other.update(Msg::Paste(CHANGED.into()));
        assert!(other.active_input_text().is_empty());
        assert!(!other.workbench.blocks_transfer_start());
        released.send(()).unwrap();
        app.sandbox_live.transfer.take().unwrap().finish();
        assert!(!crate::sandbox::transfer::active());
        assert_eq!(dispatch.claim_idle(0).len(), 1);
    }

    #[test_case(json!([]), false; "empty")]
    #[test_case(json!({"unavailable":"journal unreadable", "replay_forbidden":true}), true; "unavailable")]
    fn recovery_fails_closed_without_substring_guessing(value: serde_json::Value, required: bool) {
        assert_eq!(recovery_summary(value).1, required);
    }
}
