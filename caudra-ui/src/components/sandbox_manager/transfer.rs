use std::{collections::BTreeSet, fmt::Write as _, path::PathBuf, sync::Arc};

use caudra_agent::workspace_transfer::{
    ComparisonKind, FilePreview, TransferAction, TransferEvent,
};
use caudra_config::sandbox::{Revision, SandboxName};
use caudra_workspace::WorkspacePath;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use super::{
    EditorKey, EditorMouse, Manager, SandboxAction, SandboxManager, SnapshotState, TextEditor,
    read_only_key, view::hover_style,
};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::sandbox::{
    SandboxSnapshotRequest,
    transfer::{ComparisonView, TransferCommand, TransferLink, TransferPreview, TransferReply},
};

const NO_ROLLBACK: &str = "Recovery coverage: None. Partial publication is possible. No deletes, archives, bidirectional sync or automatic rollback.";
const HELP: &str = "Space select · s Seed / p Push / l Pull · r Review · x Execute reviewed plan · c Compare · q Reconcile (query only) · Esc cancel and await cleanup";
const MAX_DISPLAY_BYTES: usize = 128 * 1024;
const STATUS_FOCUS: usize = 2;

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
    top: usize,
    scrollbar: Scrollbar,
    action: TransferAction,
    plan: Option<Arc<TransferPreview>>,
    detail: TextEditor,
    status: String,
    status_text: TextEditor,
    status_area: Rect,
    mouse_editor: Option<TransferControl>,
    field_areas: [Rect; 2],
    list_area: Rect,
    detail_area: Rect,
    hovered: Option<TransferControl>,
    pressed: Option<TransferControl>,
}

#[derive(Clone, PartialEq, Eq)]
enum TransferControl {
    Field(usize),
    Row(usize),
    Detail,
    Status,
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
        Self { name, revision, local: TextEditor::new(), remote, focus: 0, setup: true, busy: false, connected: false, comparison: None, selected: BTreeSet::new(), row: 0, top: 0, scrollbar: Scrollbar::default(), action: TransferAction::Push, plan: None, detail: TextEditor::new(), status: "Choose both transfer roots. Local root must be absolute; remote root must exist, relative to the verified Workcell workspace root, NEVER the agent cwd. Enter compares after read permissions.".into(), status_text: TextEditor::new(), status_area: Rect::ZERO, mouse_editor: None, field_areas: [Rect::ZERO; 2], list_area: Rect::ZERO, detail_area: Rect::ZERO, hovered: None, pressed: None }
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
        if self.setup && !self.busy && self.focus != STATUS_FOCUS {
            if self.focus == 0 {
                &mut self.local
            } else {
                &mut self.remote
            }
            .handle_paste(text);
        }
    }

    fn receive(&mut self, reply: TransferReply) {
        self.reset_mouse();
        self.busy = false;
        match reply {
            TransferReply::Compared(comparison, recovery) => {
                self.status = format!(
                    "Complete: {} · {:?} · {HELP}",
                    comparison.complete(),
                    self.action
                );
                self.detail.set_text(format!("Immutable authority / instance / root scope:\n{}\n\nRecovery records (outside roots):\n{}\n{NO_ROLLBACK}\n\nComparison rows:\n{}", serde_json::to_string_pretty(comparison.context()).unwrap_or_default(), recovery, serde_json::to_string_pretty(comparison.rows()).unwrap_or_default()));
                self.comparison = Some(comparison);
                self.connected = true;
                self.setup = false;
                self.plan = None;
                self.selected.clear();
                self.row = 0;
                self.top = 0;
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
        self.status_text.set_text(self.status.clone());
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        let [body, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(area);
        if self.status_area != status {
            self.reset_mouse();
        }
        self.field_areas = [Rect::ZERO; 2];
        self.list_area = Rect::ZERO;
        self.detail_area = Rect::ZERO;
        self.status_area = status;
        if self.status_text.text() != self.status {
            if self.mouse_editor == Some(TransferControl::Status) {
                self.reset_mouse();
            }
            self.status_text.set_text(self.status.clone());
        }
        self.status_text.view_json(frame, status);
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
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(hover_style(
                        Style::default(),
                        self.hovered == Some(TransferControl::Field(index)),
                    ))
                    .title(format!(
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
            self.top = self
                .top
                .min(self.row_count().saturating_sub(usize::from(list.height)));
            let rows = self
                .comparison
                .as_ref()
                .map(|comparison| {
                    comparison
                        .rows()
                        .iter()
                        .enumerate()
                        .skip(self.top)
                        .take(list.height as usize)
                        .map(|(index, row)| {
                            Line::styled(
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
                                ),
                                hover_style(
                                    Style::default(),
                                    self.hovered == Some(TransferControl::Row(index)),
                                ),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            frame.render_widget(Paragraph::new(rows), list);
            self.scrollbar
                .draw(frame, list, self.row_count() as u32, self.top as u32);
            self.detail.view_json(frame, detail);
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) -> SandboxAction {
        let at = Position::new(event.column, event.row);
        if !self.setup && self.mouse_editor.is_none() {
            match self.scrollbar.handle(&event) {
                ScrollbarMouse::Ignored => {}
                ScrollbarMouse::Consumed => {
                    self.clear_controls();
                    return SandboxAction::None;
                }
                ScrollbarMouse::ScrollTo(top) => {
                    self.top = top as usize;
                    self.clear_controls();
                    return SandboxAction::None;
                }
            }
        }
        let editor_hit = if self.status_area.contains(at) {
            Some(TransferControl::Status)
        } else if !self.setup && self.detail_area.contains(at) {
            Some(TransferControl::Detail)
        } else if self.setup {
            self.field_areas
                .iter()
                .position(|area| area.contains(at))
                .map(TransferControl::Field)
        } else {
            None
        };
        let target = self.mouse_editor.clone().or(editor_hit);
        if let Some(target) = target {
            self.hovered = Some(target.clone());
            self.pressed = None;
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.mouse_editor = Some(target.clone());
                self.focus = match target {
                    TransferControl::Field(index) => index,
                    TransferControl::Detail => 1,
                    _ => STATUS_FOCUS,
                };
            }
            let editor = match target {
                TransferControl::Field(0) => &mut self.local,
                TransferControl::Field(_) => &mut self.remote,
                TransferControl::Detail => &mut self.detail,
                _ => &mut self.status_text,
            };
            let result = editor.handle_mouse(&event);
            if event.kind == MouseEventKind::Up(MouseButton::Left) {
                self.mouse_editor = None;
            }
            return match result {
                EditorMouse::Copy(text) => SandboxAction::Copy(text),
                _ => SandboxAction::None,
            };
        }
        let hit = if self.busy || self.setup {
            None
        } else if self.list_area.contains(at) {
            let index = self.top + usize::from(event.row - self.list_area.y);
            self.comparison
                .as_ref()
                .and_then(|comparison| comparison.rows().get(index))
                .map(|_| TransferControl::Row(index))
        } else {
            None
        };
        self.hovered = hit.clone();
        if event.kind == MouseEventKind::Moved {
            if hit.is_none() {
                self.pressed = None;
            }
            return SandboxAction::None;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            self.pressed = hit.clone();
        }
        if matches!(event.kind, MouseEventKind::Drag(_)) {
            self.pressed = None;
        }
        let activate = event.kind == MouseEventKind::Up(MouseButton::Left)
            && self.pressed.take() == hit
            && hit.is_some();
        if activate && let Some(TransferControl::Row(index)) = hit {
            self.focus = 0;
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
        SandboxAction::None
    }

    pub(super) fn reset_mouse(&mut self) {
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        if let Some(target) = self.mouse_editor.take() {
            let editor = match target {
                TransferControl::Field(0) => &mut self.local,
                TransferControl::Field(_) => &mut self.remote,
                TransferControl::Detail => &mut self.detail,
                _ => &mut self.status_text,
            };
            editor.cancel_selection();
        }
        self.scrollbar.handle(&release);
        self.clear_controls();
    }

    fn clear_controls(&mut self) {
        self.hovered = None;
        self.pressed = None;
    }

    pub fn scroll(&mut self, at: Position, delta: i32) {
        self.reset_mouse();
        if self.list_area.contains(at) {
            self.top = self.top.saturating_add_signed(-(delta as isize)).min(
                self.row_count()
                    .saturating_sub(usize::from(self.list_area.height)),
            );
        } else if self.detail_area.contains(at) {
            self.detail.scroll(delta);
        } else if self.status_area.contains(at) {
            self.status_text.scroll(delta);
        } else if self.field_areas[0].contains(at) {
            self.local.scroll(delta);
        } else if self.field_areas[1].contains(at) {
            self.remote.scroll(delta);
        }
    }

    fn row_count(&self) -> usize {
        self.comparison
            .as_ref()
            .map_or(0, |comparison| comparison.rows().len())
    }

    fn reveal_row(&mut self) {
        let height = usize::from(self.list_area.height.max(1));
        self.top = self
            .top
            .min(self.row)
            .max(self.row.saturating_sub(height - 1));
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
        if matches!(event.code, KeyCode::Tab | KeyCode::BackTab) {
            panel.focus = if event.code == KeyCode::Tab {
                (panel.focus + 1) % (STATUS_FOCUS + 1)
            } else {
                (panel.focus + STATUS_FOCUS) % (STATUS_FOCUS + 1)
            };
            return SandboxAction::None;
        }
        if read_only_key(event)
            && (panel.focus == STATUS_FOCUS
                || (!panel.setup
                    && (panel.focus == 1 || event.modifiers.contains(KeyModifiers::CONTROL))))
        {
            let editor = if panel.focus == STATUS_FOCUS {
                &mut panel.status_text
            } else {
                &mut panel.detail
            };
            let event = if matches!(event.code, KeyCode::Home | KeyCode::End) {
                KeyEvent::new(event.code, event.modifiers | KeyModifiers::CONTROL)
            } else {
                event
            };
            return match editor.handle_key(event) {
                EditorKey::Copy(text) => SandboxAction::Copy(text),
                _ => SandboxAction::None,
            };
        }
        if panel.focus == STATUS_FOCUS {
            return SandboxAction::None;
        }
        if panel.busy {
            return SandboxAction::None;
        }
        if !panel.setup
            && event
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
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
                _ => {
                    let result = if panel.focus == 0 {
                        &mut panel.local
                    } else {
                        &mut panel.remote
                    }
                    .handle_key(event);
                    if let EditorKey::Copy(text) = result {
                        return SandboxAction::Copy(text);
                    }
                }
            }
            return SandboxAction::None;
        }
        let command = match event.code {
            KeyCode::Home | KeyCode::End => {
                if panel.focus == 1
                    || panel.comparison.is_none()
                    || event.modifiers.contains(KeyModifiers::CONTROL)
                {
                    panel
                        .detail
                        .handle_key(KeyEvent::new(event.code, KeyModifiers::CONTROL));
                } else {
                    panel.row = if event.code == KeyCode::Home {
                        0
                    } else {
                        panel
                            .comparison
                            .as_ref()
                            .map_or(0, |comparison| comparison.rows().len().saturating_sub(1))
                    };
                    panel.reveal_row();
                }
                None
            }
            KeyCode::Up => {
                panel.row = panel.row.saturating_sub(1);
                panel.reveal_row();
                None
            }
            KeyCode::Down => {
                if let Some(comparison) = &panel.comparison {
                    panel.row = (panel.row + 1).min(comparison.rows().len().saturating_sub(1));
                }
                panel.reveal_row();
                None
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let page = usize::from(panel.list_area.height.max(1));
                panel.row = if event.code == KeyCode::PageUp {
                    panel.row.saturating_sub(page)
                } else {
                    panel
                        .row
                        .saturating_add(page)
                        .min(panel.row_count().saturating_sub(1))
                };
                panel.reveal_row();
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
            panel.reset_mouse();
            panel.status = format!("{event:?} · Esc cancel + await cleanup");
            panel.status_text.set_text(panel.status.clone());
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
    use crate::components::scrollbar;
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
    use caudra_workbench::scroll::SCROLLBAR_THUMB;
    use caudra_workcell::TransferReport;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, OperationId, ProjectIdentity,
        ProjectKey, ResourceId, ResourceRevision, ResourceScope, SessionBindingId,
        SessionWorkspaceBinding, SourceTrustAnchor, TransferContent, TransferDigest, TransferMode,
        WorkspaceCursor, WorkspacePath,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};
    use serde_json::json;
    use std::{env, path::Path, process::Command, sync::Arc};
    use test_case::test_case;

    const FILE: &str = "new/deep/file.txt";
    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CHANGED_SOURCE: &str = "Source changed after preparation; compare again";
    const CHANGED_DESTINATION: &str = "Destination changed after preparation; compare again";
    const DENIED: &str = "Native permission denied";
    const UNICODE_JSON: &str = "{\"path\":\"文件/é/long name\",\"result\":\"unchanged\"}";
    const DISABLED_TEST_PROCESS: &str = "CAUDRA_TRANSFER_DISABLED_TEST_PROCESS";

    #[test_case(0; "local_root")]
    #[test_case(1; "remote_root")]
    #[test_case(2; "detail")]
    #[test_case(3; "status")]
    fn each_editor_has_one_inert_scrollbar(surface: usize) {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        panel.setup = surface < 2;
        let source = format!("{UNICODE_JSON}\n").repeat(30);
        panel.local.set_text(source.clone());
        panel.remote.set_text(source.clone());
        panel.detail.set_text(source.clone());
        panel.status = source.clone();
        let mut terminal = Terminal::new(TestBackend::new(40, 18)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let area = match surface {
            0 | 1 => panel.field_areas[surface],
            2 => panel.detail_area,
            _ => panel.status_area,
        };
        assert_eq!(
            terminal.backend().buffer()[(area.right() - 1, area.y)].symbol(),
            SCROLLBAR_THUMB
        );
        for y in area.y..area.bottom() {
            for x in area.x..area.right() - 1 {
                assert_ne!(
                    terminal.backend().buffer()[(x, y)].symbol(),
                    SCROLLBAR_THUMB
                );
            }
        }
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.right() - 1,
            row: area.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(panel.mouse(at), SandboxAction::None));
        assert!(matches!(
            panel.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            SandboxAction::None
        ));
        assert!(panel.selected.is_empty());
        assert_eq!(panel.local.text(), source);
        assert_eq!(panel.remote.text(), source);
        assert_eq!(panel.detail.text(), source);
        assert_eq!(panel.status_text.text(), source);
    }

    #[test]
    fn disabled_transfer_bars_leave_all_editor_content_intact() {
        if env::var_os(DISABLED_TEST_PROCESS).is_none() {
            assert!(Command::new(env::current_exe().unwrap())
                .args(["--exact", "components::sandbox_manager::transfer::tests::disabled_transfer_bars_leave_all_editor_content_intact"])
                .env(DISABLED_TEST_PROCESS, "1").status().unwrap().success());
            return;
        }
        scrollbar::set_enabled(false);
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        let source = format!("{UNICODE_JSON}\n").repeat(30);
        panel.local.set_text(source.clone());
        panel.remote.set_text(source.clone());
        panel.detail.set_text(source.clone());
        panel.status = source.clone();
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        for setup in [false, true] {
            panel.setup = setup;
            terminal
                .draw(|frame| panel.view(frame, frame.area()))
                .unwrap();
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| cell.symbol() != SCROLLBAR_THUMB)
            );
        }
        assert_eq!(panel.local.text(), source);
        assert_eq!(panel.remote.text(), source);
        assert_eq!(panel.detail.text(), source);
        assert_eq!(panel.status_text.text(), source);
    }

    #[test]
    fn detail_drag_copies_offscreen_unicode_without_changing_authority() {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        let source = format!("{}终点", "文件 é\n".repeat(30));
        panel.detail.set_text(source.clone());
        let authority = panel.comparison.clone().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: panel.detail_area.x,
            row: panel.detail_area.y,
            modifiers: KeyModifiers::NONE,
        };
        panel.mouse(at);
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: panel.detail_area.right() - 2,
            row: panel.detail_area.bottom(),
            ..at
        };
        for _ in 0..source.lines().count() {
            assert!(matches!(panel.mouse(drag), SandboxAction::None));
            terminal
                .draw(|frame| panel.view(frame, frame.area()))
                .unwrap();
        }
        let SandboxAction::Copy(copied) = panel.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..drag
        }) else {
            panic!("drag should copy")
        };
        assert_eq!(copied, source);
        assert_eq!(panel.detail.text(), source);
        assert!(Arc::ptr_eq(panel.comparison.as_ref().unwrap(), &authority));
        assert!(panel.selected.is_empty());
    }

    #[test_case(0; "resize")]
    #[test_case(1; "close_reset")]
    #[test_case(2; "new_result")]
    fn interrupted_detail_drag_cannot_copy_on_late_release(interrupt: u8) {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        panel.detail.set_text(UNICODE_JSON.into());
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: panel.detail_area.x,
            row: panel.detail_area.y,
            modifiers: KeyModifiers::NONE,
        };
        panel.mouse(at);
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: at.column + 5,
            ..at
        });
        match interrupt {
            0 => {
                let mut smaller = Terminal::new(TestBackend::new(40, 12)).unwrap();
                smaller
                    .draw(|frame| panel.view(frame, frame.area()))
                    .unwrap();
            }
            1 => panel.reset_mouse(),
            _ => panel.receive(TransferReply::Reviewed(preview(
                directory.path(),
                TransferAction::Push,
                false,
            ))),
        }
        assert!(panel.mouse_editor.is_none());
        assert!(matches!(
            panel.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            SandboxAction::None
        ));
        assert!(panel.selected.is_empty());
        if interrupt == 2 {
            assert!(!panel.detail.text().contains(UNICODE_JSON));
            assert!(!matches!(
                panel
                    .detail
                    .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                super::EditorKey::Copy(_)
            ));
        }
    }

    #[test_case(0, 0; "zero")]
    #[test_case(1, 1; "single_cell")]
    #[test_case(3, 8; "single_column_panes")]
    #[test_case(8, 3; "status_only")]
    fn transfer_tiny_viewports_preserve_sources(width: u16, height: u16) {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        panel.detail.set_text(UNICODE_JSON.into());
        panel.status = UNICODE_JSON.into();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        assert_eq!(panel.detail.text(), UNICODE_JSON);
        assert_eq!(panel.status_text.text(), UNICODE_JSON);
        panel.setup = true;
        panel.local.set_text(UNICODE_JSON.into());
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        assert_eq!(panel.local.text(), UNICODE_JSON);
        assert!(panel.selected.is_empty());
    }

    #[test_case(false; "detail")]
    #[test_case(true; "status")]
    fn read_only_selection_copies_unicode_without_mutating_authority(status: bool) {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let state = manager.state.as_mut().unwrap();
        let panel = state.transfer.as_mut().unwrap();
        let comparison = panel.comparison.clone().unwrap();
        panel.detail.set_text(UNICODE_JSON.into());
        panel.status = UNICODE_JSON.into();
        panel.focus = if status { super::STATUS_FOCUS } else { 1 };
        panel.busy = true;
        let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        assert!(matches!(
            state.transfer_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            SandboxAction::None
        ));
        let SandboxAction::Copy(text) =
            state.transfer_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("selection should copy")
        };
        assert_eq!(text, UNICODE_JSON);
        state.transfer_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        let panel = state.transfer.as_ref().unwrap();
        assert_eq!(panel.detail.text(), UNICODE_JSON);
        assert_eq!(panel.status, UNICODE_JSON);
        assert!(Arc::ptr_eq(panel.comparison.as_ref().unwrap(), &comparison));
        assert!(panel.selected.is_empty());
    }

    #[test_case(false; "detail")]
    #[test_case(true; "status")]
    fn read_only_mouse_drag_copies_and_release_over_list_does_not_toggle(status: bool) {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        panel.detail.set_text(UNICODE_JSON.into());
        panel.status = UNICODE_JSON.into();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let area = if status {
            panel.status_area
        } else {
            panel.detail_area
        };
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        panel.mouse(at);
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 5,
            ..at
        });
        assert!(matches!(
            panel.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: panel.list_area.x,
                row: panel.list_area.y,
                ..at
            }),
            SandboxAction::Copy(_)
        ));
        assert!(panel.selected.is_empty());
        assert_eq!(panel.detail.text(), UNICODE_JSON);
        assert_eq!(panel.status, UNICODE_JSON);
    }

    #[test]
    fn transfer_scrollbar_and_wheel_do_not_change_active_or_checked_rows() {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let area = panel.list_area;
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.right() - 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        panel.mouse(at);
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            row: area.bottom() + 3,
            ..at
        });
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..at
        });
        assert_eq!(panel.top, 2);
        assert_eq!(panel.row, 0);
        assert!(panel.selected.is_empty());
        panel.scroll(area.as_position(), i32::MAX);
        assert_eq!(panel.top, 0);
        panel.scroll(area.as_position(), i32::MIN);
        assert_eq!(panel.top, 2);
        press(&mut manager, KeyCode::End);
        let panel = manager.state.as_ref().unwrap().transfer.as_ref().unwrap();
        assert_eq!(panel.row, 3);
        assert_eq!(panel.top, 2);
        press(&mut manager, KeyCode::Home);
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .transfer
                .as_ref()
                .unwrap()
                .top,
            0
        );
    }

    #[test]
    fn dragging_a_comparison_row_never_toggles_it() {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: panel.list_area.x + 5,
            row: panel.list_area.y,
            modifiers: KeyModifiers::NONE,
        };
        panel.mouse(at);
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: at.column + 4,
            ..at
        });
        panel.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..at
        });
        assert!(panel.selected.is_empty());
        assert!(panel.detail.text().contains(FILE));
    }

    #[test_case(false; "focused_root_editor")]
    #[test_case(true; "selected_transfer_row")]
    fn hovered_transfer_control_has_distinct_rendered_style(row: bool) {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        if row {
            panel.selected.insert(WorkspacePath::new(FILE).unwrap());
        } else {
            panel.setup = true;
            panel.focus = 0;
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let area = if row {
            panel.list_area
        } else {
            panel.field_areas[0]
        };
        let position = if row {
            (area.x, area.y)
        } else {
            (area.x - 1, area.y)
        };
        let before = terminal.backend().buffer()[position].clone();
        let selected = panel.selected.clone();
        assert!(matches!(
            panel.mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            }),
            SandboxAction::None
        ));
        terminal
            .draw(|frame| panel.view(frame, frame.area()))
            .unwrap();
        let after = &terminal.backend().buffer()[position];
        assert_eq!(before.symbol(), after.symbol());
        assert_ne!(before.style(), after.style());
        assert!(after.modifier.contains(Modifier::UNDERLINED));
        assert_eq!(panel.selected, selected);
        assert_eq!(panel.row, 0);
        assert_eq!(panel.focus, 0);
    }

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

    #[test_case(false; "matched_release")]
    #[test_case(true; "release_elsewhere")]
    fn transfer_hover_is_inert_and_selection_requires_matching_release(mismatch: bool) {
        let (directory, _, mut manager) = fixture();
        compared(&mut manager, directory.path(), ComparisonKind::LocalOnly);
        let panel = manager.state.as_mut().unwrap().transfer.as_mut().unwrap();
        panel.list_area = Rect::new(1, 1, 40, 4);
        let mouse = |kind, row| MouseEvent {
            kind,
            column: 1,
            row,
            modifiers: KeyModifiers::NONE,
        };
        panel.mouse(mouse(MouseEventKind::Moved, 1));
        assert!(panel.hovered.is_some());
        assert!(panel.selected.is_empty());
        panel.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        assert!(panel.selected.is_empty());
        panel.mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            if mismatch { 2 } else { 1 },
        ));
        assert_eq!(panel.selected.is_empty(), mismatch);
        panel.mouse(mouse(MouseEventKind::Moved, 0));
        assert!(panel.hovered.is_none());
        press(&mut manager, KeyCode::End);
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .transfer
                .as_ref()
                .unwrap()
                .row,
            3
        );
        press(&mut manager, KeyCode::Home);
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .transfer
                .as_ref()
                .unwrap()
                .row,
            0
        );
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
