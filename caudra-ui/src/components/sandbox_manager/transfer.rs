use std::{collections::BTreeSet, fmt::Write as _, path::PathBuf, sync::Arc};

use caudra_agent::workspace_transfer::{
    ComparisonKind, FilePreview, TransferAction, TransferEvent,
};
use caudra_config::sandbox::{Revision, SandboxName};
use caudra_workspace::WorkspacePath;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use super::{
    EditorMouse, Manager, SandboxAction, SandboxManager, SnapshotState, TextEditor, read_only_key,
};
use crate::sandbox::{
    SandboxSnapshotRequest,
    transfer::{ComparisonView, TransferCommand, TransferLink, TransferPreview, TransferReply},
};

const NO_ROLLBACK: &str = "Recovery coverage: None. Partial publication is possible. No deletes, archives, bidirectional sync or automatic rollback.";
const HELP: &str = "Space select · s Seed / p Push / l Pull · r Review · x Execute reviewed plan · c Compare · q Reconcile (query only) · Esc cancel and await cleanup";
const MAX_DISPLAY_BYTES: usize = 128 * 1024;
const ROW_CONTEXT: usize = 3;

pub(super) struct TransferPanel {
    name: SandboxName,
    revision: Revision,
    local: TextEditor,
    remote: TextEditor,
    focus: usize,
    setup: bool,
    pub busy: bool,
    connected: bool,
    comparison: Option<Arc<ComparisonView>>,
    selected: BTreeSet<WorkspacePath>,
    row: usize,
    action: TransferAction,
    plan: Option<Arc<TransferPreview>>,
    detail: TextEditor,
    status: String,
    field_areas: [Rect; 2],
    list_area: Rect,
    detail_area: Rect,
}

impl TransferPanel {
    pub(super) fn seed(
        name: SandboxName,
        revision: Revision,
        local: PathBuf,
        remote: WorkspacePath,
    ) -> Self {
        let mut panel = Self::new(name, revision);
        panel.local.set_text(local.to_string_lossy().into_owned());
        panel.remote.set_text(remote.to_string());
        panel.action = TransferAction::Seed;
        panel.status = "New sandbox identity verified. Initial seed=ask: Enter compares these explicit roots, then select files and Review. No source export until Execute and both-end permission approval.".into();
        panel
    }
    fn new(name: SandboxName, revision: Revision) -> Self {
        let mut remote = TextEditor::new();
        remote.set_text(".".into());
        Self { name, revision, local: TextEditor::new(), remote, focus: 0, setup: true, busy: false, connected: false, comparison: None, selected: BTreeSet::new(), row: 0, action: TransferAction::Push, plan: None, detail: TextEditor::new(), status: "Choose both transfer roots. Local root must be absolute; remote root must exist, relative to the verified Workcell workspace root, NEVER the agent cwd. Enter compares after read permissions.".into(), field_areas: [Rect::ZERO; 2], list_area: Rect::ZERO, detail_area: Rect::ZERO }
    }

    fn link(&self, scope: &SandboxSnapshotRequest) -> Result<TransferLink, String> {
        let local_root = PathBuf::from(self.local.text());
        if !local_root.is_absolute() {
            return Err("Select an absolute local root".into());
        }
        Ok(TransferLink {
            name: self.name.clone(),
            instance_revision: self.revision.clone(),
            configuration_revision: scope.configuration_revision.clone(),
            local_root,
            remote_root: WorkspacePath::new(self.remote.text())
                .map_err(|error| error.to_string())?,
        })
    }

    fn selectable(&self, kind: &ComparisonKind) -> bool {
        match self.action {
            TransferAction::Seed => *kind == ComparisonKind::LocalOnly,
            TransferAction::Push => {
                matches!(kind, ComparisonKind::LocalOnly | ComparisonKind::Conflict)
            }
            TransferAction::Pull => {
                matches!(kind, ComparisonKind::RemoteOnly | ComparisonKind::Conflict)
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.setup && !self.busy {
            if self.focus == 0 {
                &mut self.local
            } else {
                &mut self.remote
            }
            .handle_paste(text);
        }
    }

    fn receive(&mut self, reply: TransferReply) {
        self.busy = false;
        match reply {
            TransferReply::Compared(comparison, recovery) => {
                self.status = format!(
                    "Complete: {} · {:?} · {HELP}",
                    comparison.complete(),
                    self.action
                );
                self.detail.set_text(format!("Immutable authority / instance / root scope:\n{}\n\nRecovery records (outside roots):\n{}\n{NO_ROLLBACK}", serde_json::to_string_pretty(comparison.context()).unwrap_or_default(), recovery));
                self.comparison = Some(comparison);
                self.connected = true;
                self.setup = false;
                self.plan = None;
                self.selected.clear();
                self.row = 0;
            }
            TransferReply::Reviewed(plan) => {
                let mut text = format!(
                    "Plan ID: {}\n{}\nAction {:?}\nImmutable roots:\n{}\n",
                    plan.digest().as_str(),
                    NO_ROLLBACK,
                    plan.review().action,
                    serde_json::to_string_pretty(&plan.review().context.roots).unwrap_or_default()
                );
                let mut complete_display = true;
                for file in &plan.review().files {
                    let _ = writeln!(
                        text,
                        "\n{} · {} · operation {}\nCreate {:?} directories: {:?}",
                        file.path,
                        if if plan.review().action == TransferAction::Pull {
                            file.local.is_some()
                        } else {
                            file.remote.is_some()
                        } {
                            "OVERWRITE"
                        } else {
                            "NEW"
                        },
                        file.operation_id.as_str(),
                        file.directory_side,
                        file.create_directories
                    );
                    let as_text = |preview: &Option<FilePreview>| match preview {
                        Some(FilePreview::TextPrefix { text, truncated }) => {
                            Some((text.clone(), *truncated))
                        }
                        None => Some((String::new(), false)),
                        _ => None,
                    };
                    match (as_text(&file.local_preview), as_text(&file.remote_preview)) {
                        (Some((local, a)), Some((remote, b))) => {
                            let (before, after) = if plan.review().action == TransferAction::Pull {
                                (&local, &remote)
                            } else {
                                (&remote, &local)
                            };
                            text.push_str(&caudra_diff::unified_text(
                                before,
                                after,
                                "Destination -> Source (bounded text prefixes)",
                                file.path.as_str(),
                            ));
                            let _ = writeln!(
                                text,
                                "Bounded prefix; local truncated={a}, remote truncated={b}"
                            );
                        }
                        _ => {
                            let _ = writeln!(
                                text,
                                "Local: {:?}\nRemote: {:?}",
                                file.local_preview, file.remote_preview
                            );
                        }
                    }
                    if text.len() > MAX_DISPLAY_BYTES {
                        text.truncate(text.floor_char_boundary(MAX_DISPLAY_BYTES));
                        text.push_str("\nDisplay truncated; select fewer files to inspect every preview before executing.");
                        complete_display = false;
                        break;
                    }
                }
                self.detail.set_text(text);
                self.plan = complete_display.then_some(plan);
                self.status = format!(
                    "Review shown. x requests native permissions on BOTH ends, then executes this exact plan. {HELP}"
                );
            }
            TransferReply::Finished(report) => {
                self.plan = None;
                self.comparison = None;
                self.detail
                    .set_text(serde_json::to_string_pretty(&report).unwrap_or_default());
                self.status = format!(
                    "Result {} · {}. Unknown/partial remain in recovery. q queries, NEVER replays; c creates a fresh comparison. {NO_ROLLBACK}",
                    report.result_id,
                    report.stopped.as_deref().unwrap_or("settled")
                );
            }
            TransferReply::Failed(error) => {
                self.plan = None;
                self.status = error;
            }
            TransferReply::Closed => {
                self.connected = false;
                self.plan = None;
                self.status.push_str("\nWorker settled; cleanup awaited. Esc closes; c reconnects for Compare/Reconcile. Prior results retained.");
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        self.field_areas = [Rect::ZERO; 2];
        self.list_area = Rect::ZERO;
        self.detail_area = Rect::ZERO;
        let [body, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(area);
        frame.render_widget(
            Paragraph::new(self.status.as_str()).wrap(Wrap { trim: false }),
            status,
        );
        if self.setup {
            let fields = Layout::vertical([
                Constraint::Length(4),
                Constraint::Length(4),
                Constraint::Min(1),
            ])
            .split(body);
            for (index, (editor, title)) in [
                (&mut self.local, "Explicit local root (absolute)"),
                (
                    &mut self.remote,
                    "Explicit remote root (workspace-relative)",
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let block = Block::default().borders(Borders::ALL).title(format!(
                    "{} {title}",
                    if self.focus == index { ">" } else { " " }
                ));
                let inner = block.inner(fields[index]);
                self.field_areas[index] = inner;
                frame.render_widget(block, fields[index]);
                editor.view(frame, inner);
            }
        } else {
            let [list, detail] =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .areas(body);
            self.list_area = list;
            self.detail_area = detail;
            let rows = self
                .comparison
                .as_ref()
                .map(|comparison| {
                    comparison
                        .rows()
                        .iter()
                        .enumerate()
                        .skip(self.row.saturating_sub(ROW_CONTEXT))
                        .take(list.height as usize)
                        .map(|(index, row)| {
                            format!(
                                "{} [{}] {:?} L:{} R:{} {}",
                                if index == self.row { ">" } else { " " },
                                if self.selected.contains(&row.path) {
                                    "x"
                                } else {
                                    " "
                                },
                                row.kind,
                                row.local
                                    .as_ref()
                                    .map(|stamp| stamp.content.size_bytes)
                                    .unwrap_or(0),
                                row.remote
                                    .as_ref()
                                    .map(|stamp| stamp.content.size_bytes)
                                    .unwrap_or(0),
                                row.path
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            frame.render_widget(Paragraph::new(rows), list);
            self.detail.view(frame, detail);
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) -> SandboxAction {
        let at = Position::new(event.column, event.row);
        if self.setup {
            if let Some(index) = self.field_areas.iter().position(|area| area.contains(at)) {
                self.focus = index;
                let editor = if index == 0 {
                    &mut self.local
                } else {
                    &mut self.remote
                };
                if let EditorMouse::Copy(text) = editor.handle_mouse(&event) {
                    return SandboxAction::Copy(text);
                }
            }
        } else if self.list_area.contains(at) && !self.busy {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                let index = self.row.saturating_sub(ROW_CONTEXT)
                    + usize::from(event.row - self.list_area.y);
                if let Some(row) = self
                    .comparison
                    .as_ref()
                    .and_then(|comparison| comparison.rows().get(index))
                {
                    if self.selectable(&row.kind) {
                        if !self.selected.remove(&row.path) {
                            self.selected.insert(row.path.clone());
                        }
                        self.plan = None;
                    }
                    self.row = index;
                }
            }
        } else if self.detail_area.contains(at)
            && let EditorMouse::Copy(text) = self.detail.handle_mouse(&event)
        {
            return SandboxAction::Copy(text);
        }
        SandboxAction::None
    }

    pub fn scroll(&mut self, at: Position, delta: i32) {
        if self.list_area.contains(at) {
            self.row = self.row.saturating_add_signed(-(delta as isize)).min(
                self.comparison
                    .as_ref()
                    .map_or(0, |comparison| comparison.rows().len().saturating_sub(1)),
            );
        } else if self.detail_area.contains(at) {
            self.detail.scroll(delta);
        }
    }
}

impl Manager {
    pub(super) fn open_transfer(&mut self) {
        if self.dirty() || self.pending.is_some() || self.live_pending.is_some() {
            self.status = "Save configuration and await live operations first".into();
            return;
        }
        let Some(selected) = self.entries().get(self.selected).cloned() else {
            return;
        };
        let Some(record) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| match &snapshot.instances {
                SnapshotState::Ready(rows) => {
                    rows.iter().find(|row| row.id == selected)?.record.as_ref()
                }
                _ => None,
            })
        else {
            self.status = "Save/attach the instance identity before linking a transfer".into();
            return;
        };
        match record.revision() {
            Ok(revision) => self.transfer = Some(TransferPanel::new(record.name.clone(), revision)),
            Err(error) => self.status = error.to_string(),
        }
    }

    pub(super) fn transfer_key(&mut self, event: KeyEvent) -> SandboxAction {
        if event.kind != KeyEventKind::Press {
            return SandboxAction::None;
        }
        let Some(panel) = self.transfer.as_mut() else {
            return SandboxAction::None;
        };
        if event.code == KeyCode::Esc {
            if panel.connected || panel.busy {
                panel.busy = true;
                panel.status = "Cancelling; awaiting worker cleanup. No replay or rollback.".into();
                return SandboxAction::Transfer(TransferCommand::Close);
            }
            self.transfer = None;
            return SandboxAction::None;
        }
        if panel.busy {
            return SandboxAction::None;
        }
        if panel.setup || (!panel.connected && event.code == KeyCode::Char('c')) {
            match event.code {
                KeyCode::Enter | KeyCode::Char('c')
                    if !panel.setup || event.code == KeyCode::Enter =>
                {
                    let Some(baseline) = &self.baseline else {
                        return SandboxAction::None;
                    };
                    let scope = SandboxSnapshotRequest {
                        conversation: self.conversation,
                        manager_session: self.session,
                        configuration_revision: baseline.saved().revision().clone(),
                        configuration_epoch: self.configuration_epoch,
                    };
                    match panel.link(&scope) {
                        Ok(link) => {
                            panel.busy = true;
                            return SandboxAction::Transfer(TransferCommand::Open { scope, link });
                        }
                        Err(error) => panel.status = error,
                    }
                }
                KeyCode::Tab | KeyCode::BackTab => panel.focus = 1 - panel.focus,
                _ => {
                    if panel.focus == 0 {
                        &mut panel.local
                    } else {
                        &mut panel.remote
                    }
                    .handle_key(event);
                }
            }
            return SandboxAction::None;
        }
        let command = match event.code {
            KeyCode::Up => {
                panel.row = panel.row.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                if let Some(comparison) = &panel.comparison {
                    panel.row = (panel.row + 1).min(comparison.rows().len().saturating_sub(1));
                }
                None
            }
            KeyCode::Char(' ') => {
                if let Some(row) = panel
                    .comparison
                    .as_ref()
                    .and_then(|comparison| comparison.rows().get(panel.row))
                    && panel.selectable(&row.kind)
                {
                    if !panel.selected.remove(&row.path) {
                        panel.selected.insert(row.path.clone());
                    }
                    panel.plan = None;
                }
                None
            }
            KeyCode::Char('s' | 'p' | 'l') => {
                panel.action = match event.code {
                    KeyCode::Char('s') => TransferAction::Seed,
                    KeyCode::Char('l') => TransferAction::Pull,
                    _ => TransferAction::Push,
                };
                panel.selected.clear();
                panel.plan = None;
                panel.status = format!("{:?}: select files, then r Review. {HELP}", panel.action);
                None
            }
            KeyCode::Char('r') if !panel.selected.is_empty() => Some(TransferCommand::Review(
                panel.action.clone(),
                panel.selected.iter().cloned().collect(),
            )),
            KeyCode::Char('x') => panel
                .plan
                .as_ref()
                .map(|plan| TransferCommand::Execute(plan.digest().clone())),
            KeyCode::Char('c') => {
                panel.plan = None;
                Some(TransferCommand::Compare)
            }
            KeyCode::Char('q') => {
                panel.plan = None;
                Some(TransferCommand::Reconcile)
            }
            _ => {
                if read_only_key(event) {
                    panel.detail.handle_key(event);
                }
                None
            }
        };
        if let Some(command) = command {
            panel.busy = true;
            SandboxAction::Transfer(command)
        } else {
            SandboxAction::None
        }
    }
}

impl SandboxManager {
    pub(crate) fn receive_transfer(
        &mut self,
        scope: &SandboxSnapshotRequest,
        reply: TransferReply,
    ) {
        if self.snapshot_request(scope.conversation).as_ref() != Some(scope) {
            return;
        }
        if let Some(panel) = self
            .state
            .as_mut()
            .and_then(|state| state.transfer.as_mut())
        {
            panel.receive(reply);
        }
    }

    pub(crate) fn transfer_progress(
        &mut self,
        scope: &SandboxSnapshotRequest,
        event: TransferEvent,
    ) {
        if self.snapshot_request(scope.conversation).as_ref() == Some(scope)
            && let Some(panel) = self
                .state
                .as_mut()
                .and_then(|state| state.transfer.as_mut())
        {
            panel.status = format!("{event:?} · Esc cancel + await cleanup");
        }
    }

    pub(crate) fn transfer_open(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.transfer.is_some())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::{fixture, live_instance, press};
    use super::{
        ComparisonView, NO_ROLLBACK, SandboxAction, TransferCommand, TransferPreview, TransferReply,
    };
    use crate::{
        components::{
            Overlay,
            sandbox_manager::{SandboxManager, SandboxView},
        },
        sandbox::SandboxSnapshotRequest,
    };
    use caudra_agent::workspace_transfer::{
        ComparisonKind, ComparisonRow, FilePreview, FileStamp, InventoryContext, InventoryNode,
        LocalRootIdentity, MetadataPolicy, NodeKind, PlanReview, PlannedFile, RemoteRootIdentity,
        RollbackCoverage, Side, TransferAction, TransferRoots,
    };
    use caudra_config::sandbox::Revision;
    use caudra_storage::id::CaudraId;
    use caudra_workcell::TransferReport;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, OperationId, ProjectIdentity,
        ProjectKey, ResourceId, ResourceRevision, ResourceScope, SessionBindingId,
        SessionWorkspaceBinding, SourceTrustAnchor, TransferContent, TransferDigest, TransferMode,
        WorkspaceCursor, WorkspacePath,
    };
    use crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;
    use std::{path::Path, sync::Arc};
    use test_case::test_case;

    const FILE: &str = "new/deep/file.txt";
    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CHANGED_SOURCE: &str = "Source changed after preparation; compare again";
    const CHANGED_DESTINATION: &str = "Destination changed after preparation; compare again";
    const DENIED: &str = "Native permission denied";

    fn context(local: &Path) -> InventoryContext {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("widget-transfer").unwrap(),
            "server",
            "instance",
            "generation",
            "namespace",
        )
        .unwrap();
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("binding").unwrap(),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
            ProjectIdentity::new(authority, ProjectKey::new("project").unwrap()),
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").unwrap()),
            0,
            CwdHandle::new("root-handle").unwrap(),
        );
        InventoryContext {
            roots: TransferRoots {
                local: LocalRootIdentity::capture(local).unwrap(),
                remote: RemoteRootIdentity {
                    binding,
                    cursor,
                    cwd: WorkspacePath::new("chosen-root").unwrap(),
                },
            },
            local_ignore_digest: TransferDigest::new(DIGEST).unwrap(),
            remote_ignore_digest: TransferDigest::new(DIGEST).unwrap(),
            safe_local_traversal: true,
            safe_remote_traversal: true,
        }
    }

    fn stamp() -> FileStamp {
        FileStamp {
            node: InventoryNode {
                path: WorkspacePath::new(FILE).unwrap(),
                identity: ResourceId::new("file").unwrap(),
                revision: ResourceRevision::new("revision").unwrap(),
                kind: NodeKind::File,
                size_bytes: Some(12),
                ignored: Some(false),
            },
            revision: ResourceRevision::new("revision").unwrap(),
            content: TransferContent {
                size_bytes: 12,
                digest: TransferDigest::new(DIGEST).unwrap(),
                mode: TransferMode::Regular,
            },
        }
    }

    fn compared(
        manager: &mut SandboxManager,
        local: &Path,
        kind: ComparisonKind,
    ) -> SandboxSnapshotRequest {
        live_instance(manager, false);
        assert!(matches!(
            press(manager, KeyCode::Char('t')),
            SandboxAction::None
        ));
        manager.handle_paste(&local.to_string_lossy());
        let SandboxAction::Transfer(TransferCommand::Open { scope, link }) =
            press(manager, KeyCode::Enter)
        else {
            panic!("explicit root compare");
        };
        assert_eq!(link.local_root, local);
        let rows = [
            kind,
            ComparisonKind::Excluded,
            ComparisonKind::Unsupported,
            ComparisonKind::Incomplete,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| ComparisonRow {
            path: WorkspacePath::new(if index == 0 {
                FILE.into()
            } else {
                format!("blocked-{index}")
            })
            .unwrap(),
            local: Some(stamp()),
            remote: Some(stamp()),
            local_kind: Some(NodeKind::File),
            remote_kind: Some(NodeKind::File),
            kind,
        })
        .collect();
        manager.receive_transfer(
            &scope,
            TransferReply::Compared(
                Arc::new(ComparisonView {
                    context: context(local),
                    rows,
                    complete: false,
                }),
                json!([]),
            ),
        );
        scope
    }

    fn preview(local: &Path, action: TransferAction, overwrite: bool) -> Arc<TransferPreview> {
        let pull = action == TransferAction::Pull;
        Arc::new(TransferPreview {
            digest: TransferDigest::new(DIGEST).unwrap(),
            review: PlanReview {
                context: context(local),
                filter_digest: TransferDigest::new(DIGEST).unwrap(),
                action,
                files: vec![PlannedFile {
                    operation_id: OperationId::new("operation").unwrap(),
                    path: WorkspacePath::new(FILE).unwrap(),
                    local: (!pull || overwrite).then(stamp),
                    remote: (pull || overwrite).then(stamp),
                    local_preview: Some(FilePreview::TextPrefix {
                        text: "local\n".into(),
                        truncated: false,
                    }),
                    remote_preview: Some(FilePreview::TextPrefix {
                        text: "remote\n".into(),
                        truncated: true,
                    }),
                    parents: Vec::new(),
                    create_directories: ["new", "new/deep"]
                        .into_iter()
                        .map(|path| WorkspacePath::new(path).unwrap())
                        .collect(),
                    directory_side: if pull { Side::Local } else { Side::Remote },
                }],
                metadata: MetadataPolicy::ContentAndExecutableBitOnly,
                rollback: RollbackCoverage::None,
                atomic_across_files: false,
                atomic_replace_against_external_writers: false,
            },
        })
    }

    #[test_case('s', TransferAction::Seed, ComparisonKind::LocalOnly, false; "seed_nested_remote_new")]
    #[test_case('p', TransferAction::Push, ComparisonKind::Conflict, true; "push_overwrite")]
    #[test_case('l', TransferAction::Pull, ComparisonKind::RemoteOnly, false; "pull_nested_local_new")]
    fn widget_workflow_requires_selection_review_execute_and_retains_unknowns(
        key: char,
        action: TransferAction,
        kind: ComparisonKind,
        overwrite: bool,
    ) {
        let (directory, _, mut manager) = fixture();
        let scope = compared(&mut manager, directory.path(), kind);
        assert!(matches!(
            press(&mut manager, KeyCode::Char('x')),
            SandboxAction::None
        ));
        press(&mut manager, KeyCode::Char(key));
        press(&mut manager, KeyCode::Char(' '));
        let SandboxAction::Transfer(TransferCommand::Review(actual, paths)) =
            press(&mut manager, KeyCode::Char('r'))
        else {
            panic!("review");
        };
        assert_eq!(actual, action);
        assert_eq!(paths[0].as_str(), FILE);
        assert!(matches!(
            press(&mut manager, KeyCode::Char('x')),
            SandboxAction::None
        ));
        manager.receive_transfer(
            &scope,
            TransferReply::Reviewed(preview(directory.path(), action, overwrite)),
        );
        let panel = manager.state.as_ref().unwrap().transfer.as_ref().unwrap();
        assert!(panel.detail.text().contains(NO_ROLLBACK));
        assert!(
            panel
                .detail
                .text()
                .contains(if overwrite { "OVERWRITE" } else { "NEW" })
        );
        assert!(panel.detail.text().contains("new/deep"));
        assert!(panel.detail.text().contains("truncated=true"));
        let mut terminal = Terminal::new(TestBackend::new(140, 38)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Incomplete"));
        assert!(screen.contains("L:12 R:12"));
        assert!(matches!(
            press(&mut manager, KeyCode::Char('x')),
            SandboxAction::Transfer(TransferCommand::Execute(_))
        ));
        assert!(matches!(
            press(&mut manager, KeyCode::Esc),
            SandboxAction::Transfer(TransferCommand::Close)
        ));
        manager.receive_transfer(
            &scope,
            TransferReply::Finished(Box::new(TransferReport {
                result_id: "result-a".into(),
                plan_id: Some(TransferDigest::new(DIGEST).unwrap()),
                outcomes: json!({"operation":"Unknown"}),
                stopped: Some("cancelled after dispatch".into()),
                cleanup_deferred: json!(["operation"]),
                recovery: json!([{"operation_id":"operation","state":"Unknown"}]),
                audit: json!({}),
            })),
        );
        manager.receive_transfer(&scope, TransferReply::Closed);
        assert!(matches!(
            press(&mut manager, KeyCode::Char('x')),
            SandboxAction::None
        ));
        assert!(
            manager
                .state
                .as_ref()
                .unwrap()
                .transfer
                .as_ref()
                .unwrap()
                .detail
                .text()
                .contains("Unknown")
        );
        manager.close();
        manager.open(scope.conversation, SandboxView::Instances);
        assert!(
            manager
                .state
                .as_ref()
                .unwrap()
                .transfer
                .as_ref()
                .unwrap()
                .detail
                .text()
                .contains("result-a")
        );
        assert!(matches!(
            press(&mut manager, KeyCode::Char('c')),
            SandboxAction::Transfer(TransferCommand::Open { .. })
        ));
    }

    #[test_case(CHANGED_SOURCE; "source_changed")]
    #[test_case(CHANGED_DESTINATION; "destination_changed")]
    #[test_case(DENIED; "permission_denied")]
    fn failed_prepare_invalidates_preview_without_erasing_selection(error: &str) {
        let (directory, _, mut manager) = fixture();
        let scope = compared(&mut manager, directory.path(), ComparisonKind::Conflict);
        press(&mut manager, KeyCode::Char(' '));
        press(&mut manager, KeyCode::Char('r'));
        manager.receive_transfer(
            &scope,
            TransferReply::Reviewed(preview(directory.path(), TransferAction::Push, true)),
        );
        press(&mut manager, KeyCode::Char('x'));
        manager.receive_transfer(&scope, TransferReply::Failed(error.into()));
        assert!(matches!(
            press(&mut manager, KeyCode::Char('x')),
            SandboxAction::None
        ));
        let panel = manager.state.as_ref().unwrap().transfer.as_ref().unwrap();
        assert_eq!(panel.status, error);
        assert_eq!(panel.selected.len(), 1);
    }

    #[test_case(0; "window_generation")]
    #[test_case(1; "conversation")]
    #[test_case(2; "profile_revision")]
    fn stale_reply_cannot_install_a_review(guard: usize) {
        let (directory, _, mut manager) = fixture();
        let mut scope = compared(&mut manager, directory.path(), ComparisonKind::Conflict);
        match guard {
            0 => scope.manager_session += 1,
            1 => scope.conversation = CaudraId::generate(),
            _ => scope.configuration_revision = Revision::parse(DIGEST).unwrap(),
        }
        manager.receive_transfer(
            &scope,
            TransferReply::Reviewed(preview(directory.path(), TransferAction::Push, true)),
        );
        assert!(matches!(
            press(&mut manager, KeyCode::Char('x')),
            SandboxAction::None
        ));
    }

    #[test]
    fn excluded_unsupported_incomplete_are_not_selectable_and_attach_does_not_transfer() {
        let (directory, _, mut manager) = fixture();
        live_instance(&mut manager, false);
        press(&mut manager, KeyCode::Char('a'));
        assert!(!manager.transfer_open());
        manager.state.as_mut().unwrap().live_form = None;
        compared(&mut manager, directory.path(), ComparisonKind::Excluded);
        for _ in 0..4 {
            press(&mut manager, KeyCode::Char(' '));
            press(&mut manager, KeyCode::Down);
        }
        assert!(matches!(
            press(&mut manager, KeyCode::Char('r')),
            SandboxAction::None
        ));
    }
}
