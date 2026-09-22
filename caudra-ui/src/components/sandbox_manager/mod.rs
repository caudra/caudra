mod form;
mod image;
mod live;
mod transfer;
mod view;

use super::scrollbar::{Scrollbar, ScrollbarMouse};
use super::text_editor::{EditorKey, EditorMouse, TextEditor};
use super::{HintBar, Overlay, keybindings::key};
use crate::sandbox::{
    LiveOperation, LiveOutcome, LiveReply, LiveRequest, NETWORK_SAVE_NOTICE, SandboxAttachment,
    SandboxProviderSnapshot, SandboxSnapshot, SandboxSnapshotRequest, SnapshotState, StoreEffect,
    StoreReply, StoreResult, StoreTicket,
};
use caudra_config::sandbox::persistence::LoadedSandboxes;
use caudra_config::sandbox::{
    Enforcement, MAX_SANDBOX_FILE_BYTES, MAX_SANDBOX_RECORD_BYTES, MAX_SANDBOX_RECORDS, RecordKind,
    ResourceRange, Revision, SandboxDraft, SandboxName, TlsMode,
};
use caudra_sandbox::Ownership;
use caudra_storage::id::CaudraId;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use form::Form;
use live::{LiveForm, RetainedLive};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

const UNAVAILABLE: &str = "Waiting for live provider snapshots. Configure a provider/credential, then Doctor or Refresh. No VM action was performed.";
const FUTURE_ONLY: &str = "Saved profile defaults affect future launches only; running effective configuration is unchanged.";
const CONFLICT: &str = "File changed externally. Draft preserved. Compare, Reload (discards only after confirmation), or Save as a new file.";
const KEEP_EDITING: &str =
    "Unsaved draft: Save / Discard / Keep editing. Keep editing is selected by default.";
const SAVED: &str = "Saved defaults. No VM, network, workspace or transfer action was performed.";
const BUSY: &str = "Persistence in progress; edits are retained. Wait for acknowledgment before leaving this draft.";
const TOO_LARGE: &str = "Input exceeds the sandbox editor size limit.";
const IMPORT_HELP: &str = "Paste strict versioned sandboxes.toml (credential references only). Ctrl+Enter validates and replaces the document draft, not the file.";
const EXPORT_HELP: &str = "Strict configuration preview: references only, no credentials or live instance IDs. Ctrl+S saves to a NEW private file; Ctrl+C copies a selection.";
const LOCKED_UNKNOWN: &str = "Provider capabilities/catalog unavailable: defaults can be saved offline, but launch compatibility is unverified.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SandboxView {
    Instances,
    Profiles,
    Images,
    Providers,
}

impl SandboxView {
    fn label(&self) -> &'static str {
        match self {
            Self::Instances => "Instances",
            Self::Profiles => "Profiles",
            Self::Images => "Images",
            Self::Providers => "Providers",
        }
    }
}

pub(crate) enum SandboxAction {
    None,
    Copy(String),
    Store {
        ticket: StoreTicket,
        effect: StoreEffect,
    },
    Live(Box<LiveRequest>),
    Transfer(crate::sandbox::transfer::TransferCommand),
}

#[derive(Default)]
pub(crate) struct SandboxManager {
    state: Option<Box<Manager>>,
    generation: u64,
}

#[derive(Clone)]
enum Navigation {
    Back,
    View(SandboxView),
    Select(usize),
    New,
    Duplicate,
    Policies(RecordKind),
    Import,
    Reload,
}

struct Pending {
    ticket: StoreTicket,
    after: Option<Navigation>,
}

enum Confirmation {
    Dirty {
        after: Navigation,
        choice: usize,
    },
    DeleteProfile {
        name: SandboxName,
        choice: usize,
    },
    Live {
        operation: Box<LiveOperation>,
        preview: String,
        choice: usize,
    },
    NetworkSave {
        effect: Box<StoreEffect>,
        after: Option<Navigation>,
        preview: String,
        draft_revision: u64,
        choice: usize,
    },
}

#[derive(PartialEq, Eq)]
enum Focus {
    List,
    Detail,
    Search,
}

#[derive(PartialEq, Eq)]
enum DocumentMode {
    Import,
    Export,
    Compare,
    LiveReport,
}

struct DocumentEditor {
    mode: DocumentMode,
    editor: TextEditor,
    destination: Option<TextEditor>,
    export: Option<SandboxDraft>,
}

struct ReferenceChoice {
    label: String,
    fields: Vec<(&'static str, String)>,
}

struct ReferencePicker {
    choices: Vec<ReferenceChoice>,
    search: TextEditor,
    selected: usize,
}

impl ReferencePicker {
    fn filtered(&self) -> Vec<usize> {
        let query = self.search.text().to_lowercase();
        self.choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                choice
                    .label
                    .to_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect()
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Control {
    Editor,
    View(SandboxView),
    Row(usize),
    Field(usize),
    Search,
    Confirm(usize),
    Reference(usize),
    LiveField(usize),
    InstanceActions,
    InstanceAction(usize),
}

#[derive(Default)]
struct ReadPane {
    editor: TextEditor,
    fingerprint: Option<u64>,
    area: Rect,
    visible: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadSurface {
    Body,
    Summary,
    Help,
    Status,
    Empty,
}

impl ReadSurface {
    const ALL: [Self; 5] = [
        Self::Body,
        Self::Summary,
        Self::Help,
        Self::Status,
        Self::Empty,
    ];
}

impl ReadPane {
    fn view(&mut self, frame: &mut Frame, area: Rect, text: String) {
        if self.area != area {
            self.editor.cancel_selection();
        }
        self.visible = true;
        let mut hash = DefaultHasher::new();
        text.hash(&mut hash);
        let fingerprint = hash.finish();
        if self.fingerprint != Some(fingerprint) {
            self.editor = TextEditor::new();
            self.editor.set_text(text);
            self.fingerprint = Some(fingerprint);
        }
        self.area = area;
        self.editor.view_json(frame, area);
    }
}

struct Manager {
    open: bool,
    conversation: CaudraId,
    session: u64,
    configuration_epoch: u64,
    baseline: Option<Arc<LoadedSandboxes>>,
    formatting: Option<Arc<LoadedSandboxes>>,
    draft: SandboxDraft,
    revision: u64,
    operation: u64,
    pending: Option<Pending>,
    live_pending: Option<(StoreTicket, SandboxSnapshotRequest)>,
    live_form: Option<LiveForm>,
    report_draft: Option<LiveForm>,
    retained_live: Option<RetainedLive>,
    transfer: Option<transfer::TransferPanel>,
    conflict: Option<Result<Arc<LoadedSandboxes>, String>>,
    snapshot: Option<SandboxSnapshot>,
    view: SandboxView,
    policies: Option<RecordKind>,
    form: Option<Form>,
    document: Option<DocumentEditor>,
    references: Option<ReferencePicker>,
    instance_action: Option<usize>,
    confirmation: Option<Confirmation>,
    focus: Focus,
    detail: bool,
    search: TextEditor,
    selected: usize,
    list_scroll: usize,
    reveal_selected: bool,
    detail_scroll: u16,
    status: String,
    network_report: Option<String>,
    hints: Vec<HintBar>,
    hits: Vec<(Rect, Control)>,
    pressed: Option<Control>,
    hovered: Option<Control>,
    area: Rect,
    list_area: Rect,
    editor_area: Rect,
    readers: [ReadPane; ReadSurface::ALL.len()],
    reader_focus: Option<ReadSurface>,
    reader_capture: Option<ReadSurface>,
    editor_capture: bool,
    search_capture: bool,
    list_bar: Scrollbar,
    fields_bar: Scrollbar,
    references_bar: Scrollbar,
    fields_area: Rect,
    references_area: Rect,
    reference_scroll: usize,
    reveal_reference: bool,
    live_scroll: usize,
    live_scroll_focus: usize,
}

impl SandboxManager {
    pub(crate) fn open(&mut self, conversation: CaudraId, view: SandboxView) -> SandboxAction {
        if let Some(state) = self.state.as_mut() {
            state.cancel_selections();
            state.reset_mouse();
            state.open = true;
            if state.conversation != conversation {
                if state.pending.is_some() || state.live_pending.is_some() {
                    return SandboxAction::None;
                }
                self.generation += 1;
                state.conversation = conversation;
                state.session = self.generation;
                state.snapshot = None;
            }
            if state.dirty()
                || state.pending.is_some()
                || state.live_pending.is_some()
                || state.live_form.is_some()
                || state.report_draft.is_some()
                || state.transfer.is_some()
            {
                return SandboxAction::None;
            }
            return state.navigate(Navigation::View(view));
        }
        self.generation += 1;
        let mut state = Manager {
            open: true,
            conversation,
            session: self.generation,
            configuration_epoch: 0,
            baseline: None,
            formatting: None,
            draft: SandboxDraft::new(),
            revision: 0,
            operation: 0,
            pending: None,
            live_pending: None,
            live_form: None,
            report_draft: None,
            retained_live: None,
            transfer: None,
            conflict: None,
            snapshot: None,
            view,
            policies: None,
            form: None,
            document: None,
            references: None,
            instance_action: None,
            confirmation: None,
            focus: Focus::List,
            detail: false,
            search: TextEditor::new(),
            selected: 0,
            list_scroll: 0,
            reveal_selected: true,
            detail_scroll: 0,
            status: "Loading saved sandbox configuration…".into(),
            network_report: None,
            hints: Vec::new(),
            hits: Vec::new(),
            pressed: None,
            hovered: None,
            area: Rect::ZERO,
            list_area: Rect::ZERO,
            editor_area: Rect::ZERO,
            readers: std::array::from_fn(|_| ReadPane::default()),
            reader_focus: None,
            reader_capture: None,
            editor_capture: false,
            search_capture: false,
            list_bar: Scrollbar::default(),
            fields_bar: Scrollbar::default(),
            references_bar: Scrollbar::default(),
            fields_area: Rect::ZERO,
            references_area: Rect::ZERO,
            reference_scroll: 0,
            reveal_reference: true,
            live_scroll: 0,
            live_scroll_focus: 0,
        };
        let action = state.effect(StoreEffect::Load, None);
        self.state = Some(Box::new(state));
        action
    }

    #[cfg(test)]
    pub(crate) fn allocated(&self) -> bool {
        self.state.is_some()
    }

    pub(crate) fn dirty(&self) -> bool {
        self.state.as_ref().is_some_and(|state| state.dirty())
    }

    pub(crate) fn pending(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.pending.is_some() || state.live_pending.is_some())
    }

    pub(crate) fn baseline(&self) -> Option<Arc<LoadedSandboxes>> {
        self.state.as_ref()?.baseline.clone()
    }

    pub(crate) fn live_failed(&mut self, message: String) {
        if let Some(state) = self.state.as_mut() {
            state.live_pending = None;
            state.live_report(message);
        }
    }

    pub(crate) fn receive_live(&mut self, reply: LiveReply) -> Option<SandboxAttachment> {
        if let Some(state) = self.state.as_mut() {
            state.reset_mouse();
        }
        if self.snapshot_request(reply.scope.conversation).as_ref() != Some(&reply.scope) {
            return None;
        }
        let state = self.state.as_mut()?;
        if state.live_pending.as_ref() != Some(&(reply.ticket.clone(), reply.scope)) {
            return None;
        }
        state.live_pending = None;
        state.cancel_selections();
        if let Ok(LiveOutcome::ImageProbe(probe)) = reply.result {
            if let Some(form) = state.live_form.as_mut() {
                form.install_probe(probe);
                state.status = "Image probe complete. Review SHA-256, disk size and every operator-declared feature; Ctrl+Enter reviews import separately. Daemon must be stopped; no third-party process can be stopped here.".into();
            }
            return None;
        }
        state.configuration_epoch += 1;
        state.snapshot = None;
        match reply.result {
            Ok(LiveOutcome::ImageProbe(_)) => None,
            Ok(LiveOutcome::Seed {
                name,
                revision,
                local,
                remote,
            }) => {
                state.live_form = None;
                state.document = None;
                state.transfer = Some(transfer::TransferPanel::seed(name, revision, local, remote));
                state.status = "Workcell identity verified. Initial seed asks for a separate file review and both-end permissions; nothing exported.".into();
                None
            }
            Ok(LiveOutcome::Attachment(attachment)) => {
                if !state.open {
                    state.status = "Workcell verified, but the manager was closed. Workspace unchanged; reopen and explicitly Attach again.".into();
                    return None;
                }
                state.status = "Workcell verified. Checking session transition gates…".into();
                Some(attachment)
            }
            Ok(LiveOutcome::Report(report)) => {
                state.live_form = None;
                state.live_report(report);
                state.status = "Action report. Esc returns; no automatic attachment, VM stop or local fallback.".into();
                None
            }
            Err(error) => {
                state.live_report(error);
                None
            }
        }
    }

    pub(crate) fn snapshot_request(
        &self,
        conversation: CaudraId,
    ) -> Option<SandboxSnapshotRequest> {
        let state = self.state.as_ref()?;
        if state.conversation != conversation {
            return None;
        }
        Some(SandboxSnapshotRequest {
            conversation,
            manager_session: state.session,
            configuration_revision: state.baseline.as_ref()?.saved().revision().clone(),
            configuration_epoch: state.configuration_epoch,
        })
    }

    pub(crate) fn install_snapshot(
        &mut self,
        request: &SandboxSnapshotRequest,
        snapshot: SandboxSnapshot,
    ) -> bool {
        if self.snapshot_request(request.conversation).as_ref() != Some(request) {
            return false;
        }
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        if state
            .snapshot
            .as_ref()
            .is_some_and(|old| old.sequence >= snapshot.sequence)
            || snapshot.providers.len() > MAX_SANDBOX_RECORDS
            || matches!(&snapshot.instances, SnapshotState::Ready(rows) if rows.len() > MAX_SANDBOX_RECORDS)
        {
            return false;
        }
        let Some(baseline) = state.baseline.as_ref() else {
            return false;
        };
        if snapshot.providers.iter().any(|(name, provider)| {
            !baseline
                .saved()
                .record(RecordKind::Provider, name)
                .is_ok_and(|record| record.revision() == &provider.capabilities.provider_revision)
        }) {
            return false;
        }
        let selected = state.entries().get(state.selected).cloned();
        state.cancel_selections();
        state.reset_mouse();
        state.snapshot = Some(snapshot);
        if let Some(selected) = selected
            .and_then(|selected| state.entries().iter().position(|entry| entry == &selected))
        {
            state.selected = selected;
        }
        state.refresh_locks();
        true
    }

    pub(crate) fn receive_network_report(&mut self, report: String) {
        if let Some(state) = self.state.as_mut() {
            state.cancel_selections();
            state.network_report = Some(report);
            state.status = "Saved-network results available in Instances. Saved is not necessarily enforced; /sandbox reconcile-network inspects before retrying.".into();
        }
    }

    pub(crate) fn receive(&mut self, reply: StoreReply) -> SandboxAction {
        if let Some(state) = self.state.as_mut() {
            state.reset_mouse();
        }
        let Some(state) = self.state.as_mut() else {
            return SandboxAction::None;
        };
        if state.session != reply.ticket.session
            || !state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.ticket == reply.ticket)
        {
            return SandboxAction::None;
        }
        let pending = state.pending.take();
        let unchanged = reply.ticket.draft_revision == state.revision;
        match reply.result {
            StoreResult::Loaded(loaded) if unchanged => {
                state.formatting = None;
                state.draft = loaded.draft();
                state.baseline = Some(loaded);
                state.conflict = None;
                state.snapshot = None;
                state.configuration_epoch += 1;
                state.status = FUTURE_ONLY.into();
                state.inspect();
            }
            StoreResult::Loaded(loaded) => {
                state.conflict = Some(Ok(loaded));
                state.status = CONFLICT.into();
            }
            StoreResult::Saved(saved) => {
                let networks_changed = state.baseline.as_ref().is_some_and(|baseline| {
                    baseline.saved().configuration().networks
                        != saved.saved().configuration().networks
                });
                state.formatting = None;
                state.draft = saved.draft();
                state.baseline = Some(saved);
                state.conflict = None;
                state.snapshot = None;
                state.configuration_epoch += 1;
                if unchanged {
                    let name = state
                        .form
                        .as_ref()
                        .and_then(|form| SandboxName::parse(&form.text("name")).ok());
                    state.document = None;
                    if let (Some(form), Some(name)) = (&state.form, name) {
                        state.form =
                            state
                                .draft
                                .get(form.kind.clone(), &name)
                                .ok()
                                .and_then(|record| {
                                    Form::new(record.kind(), Some(name), Some(record)).ok()
                                });
                    }
                    state.status = if networks_changed {
                        NETWORK_SAVE_NOTICE
                    } else {
                        SAVED
                    }
                    .into();
                    if let Some(after) = pending.and_then(|pending| pending.after) {
                        return state.go(after);
                    }
                } else {
                    if let Some(form) = state.form.as_mut() {
                        form.rebase(&state.draft);
                    }
                    state.status =
                        "Saved the submitted revision; newer edits remain unsaved.".into();
                }
            }
            StoreResult::Conflict(latest) => {
                state.conflict = Some(latest.map_err(|error| error.to_string()));
                state.status = CONFLICT.into();
            }
            StoreResult::Exported => {
                state.document = None;
                state.status =
                    "Exported to a new private file. Original baseline and draft are unchanged."
                        .into();
            }
            StoreResult::Failed(error) => state.status = error.to_string(),
        }
        SandboxAction::None
    }

    pub(crate) fn disconnected(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.pending = None;
            state.status = "Persistence worker disconnected; draft retained. Reload/compare before retrying an uncertain save.".into();
        }
    }

    pub(crate) fn handle_key(&mut self, event: KeyEvent) -> SandboxAction {
        self.state
            .as_mut()
            .filter(|state| state.open)
            .map_or(SandboxAction::None, |state| state.handle_key(event))
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        let Some(state) = self.state.as_mut().filter(|state| state.open) else {
            return false;
        };
        if state.confirmation.is_none() && state.reader_focus.is_none() {
            state.paste(text);
        }
        true
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> SandboxAction {
        self.state
            .as_mut()
            .filter(|state| state.open)
            .map_or(SandboxAction::None, |state| state.mouse(event))
    }

    pub(crate) fn scroll_at(&mut self, position: Position, delta: i32) {
        if let Some(state) = self.state.as_mut().filter(|state| state.open) {
            if state.search_capture
                || (state.focus == Focus::Search
                    && state.hits.iter().any(|(area, control)| {
                        *control == Control::Search && area.contains(position)
                    }))
            {
                state.search.scroll(delta);
                return;
            }
            if let Some(reader) = state
                .readers
                .iter_mut()
                .find(|reader| reader.area.contains(position))
            {
                reader.editor.scroll(delta);
                return;
            }
            state.reset_mouse();
            if let Some(panel) = state.transfer.as_mut() {
                panel.scroll(position, delta);
                return;
            }
            if state.confirmation.is_some() {
                state.detail_scroll = state
                    .detail_scroll
                    .saturating_add_signed((-delta).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
                return;
            }
            if let Some(form) = state.live_form.as_mut() {
                if let Some(picker) = form.picker.as_mut() {
                    picker.scroll_at(position, delta);
                } else if state.fields_area.contains(position) {
                    state.live_scroll = state
                        .live_scroll
                        .saturating_add_signed(-(delta as isize))
                        .min(
                            form.fields
                                .len()
                                .saturating_sub(state.fields_area.height as usize),
                        );
                } else if let Some(field) = form.fields.get_mut(form.focus) {
                    field.editor.scroll(delta);
                }
            } else if state.instance_action.is_some() {
                state.reveal_reference = false;
                state.reference_scroll = state
                    .reference_scroll
                    .saturating_add_signed(-(delta as isize))
                    .min(
                        live::INSTANCE_ACTIONS
                            .len()
                            .saturating_sub(state.references_area.height as usize),
                    );
            } else if let Some(picker) = state.references.as_mut() {
                state.reveal_reference = false;
                state.reference_scroll = state
                    .reference_scroll
                    .saturating_add_signed(-(delta as isize))
                    .min(
                        picker
                            .filtered()
                            .len()
                            .saturating_sub(state.references_area.height as usize),
                    );
            } else if let Some(document) = state.document.as_mut() {
                document
                    .destination
                    .as_mut()
                    .unwrap_or(&mut document.editor)
                    .scroll(delta);
            } else if state.editor_area.contains(position)
                && state.form.as_ref().is_some_and(|form| form.editing)
            {
                if let Some(form) = state.form.as_mut() {
                    form.fields[form.focus].editor.scroll(delta);
                }
            } else if state.list_area.contains(position) {
                state.reveal_selected = false;
                state.list_scroll = state
                    .list_scroll
                    .saturating_add_signed(-(delta as isize))
                    .min(
                        state
                            .entries()
                            .len()
                            .saturating_sub(state.list_area.height.saturating_sub(1) as usize),
                    );
            } else if let Some(form) = state.form.as_mut() {
                form.reveal_focus = false;
                form.scroll = form.scroll.saturating_add_signed(-(delta as isize)).min(
                    form.fields
                        .len()
                        .saturating_sub(state.fields_area.height as usize),
                );
            } else {
                state.detail_scroll = state
                    .detail_scroll
                    .saturating_add_signed((-delta).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            }
        }
    }
}

impl Overlay for SandboxManager {
    fn is_open(&self) -> bool {
        self.state.as_ref().is_some_and(|state| state.open)
    }
    fn close(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.open = false;
            state.cancel_selections();
            state.reset_mouse();
        }
    }
}

impl Manager {
    fn live_report(&mut self, report: String) {
        self.cancel_selections();
        self.status = report.clone();
        if self.live_form.is_some() && self.live_pending.is_some() {
            return;
        }
        self.reset_mouse();
        if let Some(form) = self.live_form.take() {
            self.report_draft = Some(form);
        }
        let mut editor = TextEditor::new();
        let destination = if self.report_draft.is_some() {
            "the unchanged draft"
        } else {
            "the manager"
        };
        if self.report_draft.is_some() {
            self.status = format!(
                "Action report · Esc returns to {destination}. No automatic retry or attachment."
            );
        }
        editor.set_text(format!("{report}\n\nEsc returns to {destination}. No automatic retry or attachment. If the outcome is unknown, Reconcile before any new Create."));
        self.document = Some(DocumentEditor {
            mode: DocumentMode::LiveReport,
            editor,
            destination: None,
            export: None,
        });
    }

    fn dirty(&self) -> bool {
        self.formatting.is_some()
            || self.form.as_ref().is_some_and(Form::dirty)
            || self.document.as_ref().is_some_and(|document| {
                document.mode == DocumentMode::Import && !document.editor.text().is_empty()
            })
            || self
                .baseline
                .as_ref()
                .is_some_and(|baseline| baseline.saved().configuration() != &self.draft)
    }

    fn kind(&self) -> Option<RecordKind> {
        match self.view {
            SandboxView::Profiles => Some(self.policies.clone().unwrap_or(RecordKind::Profile)),
            SandboxView::Providers => Some(RecordKind::Provider),
            _ => None,
        }
    }

    fn entries(&self) -> Vec<String> {
        let entries: Vec<String> = match self.kind() {
            Some(RecordKind::Profile) => self
                .draft
                .profiles
                .keys()
                .map(ToString::to_string)
                .collect(),
            Some(RecordKind::Provider) => self
                .draft
                .providers
                .keys()
                .map(ToString::to_string)
                .collect(),
            Some(RecordKind::Network) => self
                .draft
                .networks
                .keys()
                .map(ToString::to_string)
                .collect(),
            Some(RecordKind::Transfer) => self
                .draft
                .transfers
                .keys()
                .map(ToString::to_string)
                .collect(),
            None => match (&self.view, &self.snapshot) {
                (SandboxView::Instances, Some(snapshot)) => match &snapshot.instances {
                    SnapshotState::Ready(rows) => rows.iter().map(|row| row.id.clone()).collect(),
                    _ => Vec::new(),
                },
                (SandboxView::Images, Some(snapshot)) => snapshot
                    .providers
                    .iter()
                    .flat_map(|(name, provider)| match &provider.catalog {
                        SnapshotState::Ready(catalog) => catalog
                            .entries()
                            .map(|entry| {
                                format!("{name}: {} @ {}", entry.id, entry.revision.as_str())
                            })
                            .collect(),
                        _ => Vec::new(),
                    })
                    .collect(),
                _ => Vec::new(),
            },
        };
        let query = self.search.text().to_lowercase();
        entries
            .into_iter()
            .filter(|entry| entry.to_lowercase().contains(&query))
            .collect()
    }

    fn inspect(&mut self) {
        self.cancel_selections();
        self.form = None;
        self.detail_scroll = 0;
        let entries = self.entries();
        self.selected = self.selected.min(entries.len().saturating_sub(1));
        if let (Some(kind), Some(name)) = (self.kind(), entries.get(self.selected)) {
            let result = SandboxName::parse(name)
                .map_err(|error| error.to_string())
                .and_then(|name| {
                    self.draft
                        .get(kind.clone(), &name)
                        .map_err(|error| error.to_string())
                        .and_then(|record| Form::new(kind, Some(name), Some(record)))
                });
            match result {
                Ok(form) => self.form = Some(form),
                Err(error) => self.status = error,
            }
        }
        self.validate();
    }

    fn candidate(&mut self) -> Result<SandboxDraft, String> {
        let draft = if let Some(document) = self
            .document
            .as_ref()
            .filter(|document| document.mode == DocumentMode::Import)
        {
            SandboxDraft::import(&document.editor.text()).map_err(|error| error.to_string())?
        } else if let Some(form) = self.form.as_mut() {
            form.validate(&self.draft)?
        } else {
            self.draft.validate().map_err(|error| error.to_string())?;
            self.draft.clone()
        };
        if let Err((key, message)) = self.capability_error(&draft) {
            if let Some(form) = self.form.as_mut() {
                form.set_error(key, &message);
            }
            return Err(message);
        }
        Ok(draft)
    }

    fn validate(&mut self) {
        self.refresh_locks();
        match self.candidate() {
            Err(error) => self.status = error,
            Ok(_) if self.conflict.is_some() => self.status = CONFLICT.into(),
            Ok(_) if self.pending.is_none() && self.document.is_none() => {
                self.status = FUTURE_ONLY.into()
            }
            Ok(_) => {}
        }
    }

    fn provider(
        &self,
        name: &SandboxName,
        draft: &SandboxDraft,
    ) -> Option<&SandboxProviderSnapshot> {
        let baseline = self.baseline.as_ref()?;
        if baseline.saved().configuration().providers.get(name) != draft.providers.get(name) {
            return None;
        }
        self.snapshot.as_ref()?.providers.get(name)
    }

    fn capability_error(&self, draft: &SandboxDraft) -> Result<(), (&'static str, String)> {
        for (name, profile) in &draft.profiles {
            let Some(provider) = self.provider(&profile.provider, draft) else {
                continue;
            };
            let caps = &provider.capabilities;
            let error = |key, reason| (key, format!("Profile {name}: {reason}"));
            for (key, value, range) in [
                ("cpus", profile.cpus, &caps.cpus),
                ("memory_mib", profile.memory_mib, &caps.memory_mib),
                ("disk_gib", profile.disk_gib, &caps.disk_gib),
            ] {
                if value < range.min
                    || value > range.max
                    || !(value.get() - range.min.get()).is_multiple_of(range.step.get())
                {
                    return Err(error(
                        key,
                        format!(
                            "provider requires {}..={} step {}",
                            range.min, range.max, range.step
                        ),
                    ));
                }
            }
            if profile.persistent && !caps.persistent {
                return Err(error(
                    "persistent",
                    "provider does not support persistent disks".into(),
                ));
            }
            if profile.running_ttl_seconds > caps.max_ttl_seconds {
                return Err(error(
                    "running_ttl_seconds",
                    format!("provider TTL maximum is {} seconds", caps.max_ttl_seconds),
                ));
            }
            let Some(network) = draft.networks.get(&profile.network) else {
                continue;
            };
            if !caps.network_modes.contains(&network.enforcement) {
                return Err(error(
                    "enforcement",
                    "network topology is unsupported".into(),
                ));
            }
            if network.enforcement == Enforcement::Required
                && !caps.tls_modes.contains(&network.tls_mode)
            {
                return Err(error("tls_mode", "TLS mode is unsupported".into()));
            }
            if let SnapshotState::Ready(catalog) = &provider.catalog {
                let Some(template) = catalog.get(&profile.template, &profile.template_revision)
                else {
                    return Err(error(
                        "template_revision",
                        "immutable template revision is missing from the catalog".into(),
                    ));
                };
                if !template.workcell_compatible || template.architecture != caps.architecture {
                    return Err(error(
                        "template",
                        "image architecture or Workcell compatibility is unsupported".into(),
                    ));
                }
                for (key, value, minimum) in [
                    ("cpus", profile.cpus, template.minimum_resources.cpus),
                    (
                        "memory_mib",
                        profile.memory_mib,
                        template.minimum_resources.memory_mib,
                    ),
                    (
                        "disk_gib",
                        profile.disk_gib,
                        template.minimum_resources.disk_gib,
                    ),
                ] {
                    if value < minimum {
                        return Err(error(key, format!("image minimum is {minimum}")));
                    }
                }
                if !caps.disk_growth && profile.disk_gib != template.minimum_resources.disk_gib {
                    return Err(error("disk_gib", "image disk growth is unsupported".into()));
                }
                if !template.network_modes.contains(&network.enforcement) {
                    return Err(error(
                        "enforcement",
                        "image network topology is unsupported".into(),
                    ));
                }
                if network.tls_mode == TlsMode::Mitm && !template.guest_ca {
                    return Err(error(
                        "tls_mode",
                        "MITM requires a compatible guest CA; certificate pinning can break".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn refresh_locks(&mut self) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.kind == RecordKind::Network {
            let mut locks = Vec::new();
            for profile in self
                .draft
                .profiles
                .values()
                .filter(|profile| profile.network.as_str() == form.text("name"))
            {
                let Some(provider) = self.provider(&profile.provider, &self.draft) else {
                    continue;
                };
                let caps = &provider.capabilities;
                let template = match &provider.catalog {
                    SnapshotState::Ready(catalog) => {
                        catalog.get(&profile.template, &profile.template_revision)
                    }
                    _ => None,
                };
                let enforcement = form.text("enforcement");
                if matches!(
                    (caps.network_modes.as_slice(), enforcement.as_str()),
                    ([Enforcement::Required], "required") | ([Enforcement::Off], "off")
                ) {
                    locks.push((
                        "enforcement",
                        "Locked: an affected provider supports only this network topology.",
                    ));
                }
                if form.text("tls_mode") == "sni-only"
                    && (!caps.tls_modes.contains(&TlsMode::Mitm)
                        || template.is_some_and(|template| !template.guest_ca))
                {
                    locks.push(("tls_mode", "Locked: an affected provider/image cannot use MITM (guest CA or TLS capability missing)."));
                }
            }
            if let Some(form) = self.form.as_mut() {
                for field in &mut form.fields {
                    if matches!(field.key, "enforcement" | "tls_mode") {
                        field.locked = locks
                            .iter()
                            .find(|(key, _)| *key == field.key)
                            .map(|(_, reason)| (*reason).into());
                    }
                }
            }
            return;
        }
        if form.kind != RecordKind::Profile {
            return;
        }
        let provider = SandboxName::parse(&form.text("provider"))
            .ok()
            .and_then(|name| self.provider(&name, &self.draft))
            .cloned();
        let disk_minimum = provider
            .as_ref()
            .and_then(|provider| match &provider.catalog {
                SnapshotState::Ready(catalog) => {
                    let id = SandboxName::parse(&form.text("template")).ok()?;
                    let revision = Revision::parse(&form.text("template_revision")).ok()?;
                    catalog
                        .get(&id, &revision)
                        .map(|template| template.minimum_resources.disk_gib)
                }
                _ => None,
            });
        let Some(form) = self.form.as_mut() else {
            return;
        };
        for field in &mut form.fields {
            if !matches!(field.key, "cpus" | "memory_mib" | "disk_gib" | "persistent") {
                continue;
            }
            field.locked = provider.as_ref().and_then(|provider| {
                let caps = &provider.capabilities;
                let range: Option<&ResourceRange> = match field.key {
                    "cpus" => Some(&caps.cpus),
                    "memory_mib" => Some(&caps.memory_mib),
                    "disk_gib" => Some(&caps.disk_gib),
                    _ => None,
                };
                if let Some(range) = range
                    .filter(|range| range.min == range.max && field.text() == range.min.to_string())
                {
                    Some(format!(
                        "Locked: provider fixes this resource at {} (no overrides).",
                        range.min
                    ))
                } else if field.key == "persistent" && !caps.persistent && field.text() == "false" {
                    Some("Locked: this provider does not support persistent disks.".into())
                } else if field.key == "disk_gib"
                    && !caps.disk_growth
                    && disk_minimum.is_some_and(|minimum| field.text() == minimum.to_string())
                {
                    Some("Locked: this provider cannot grow the immutable image's disk.".into())
                } else {
                    None
                }
            });
        }
    }

    fn effect(&mut self, effect: StoreEffect, after: Option<Navigation>) -> SandboxAction {
        if self.pending.is_some() {
            self.status = BUSY.into();
            return SandboxAction::None;
        }
        self.operation += 1;
        let ticket = StoreTicket {
            session: self.session,
            operation: self.operation,
            draft_revision: self.revision,
        };
        self.pending = Some(Pending {
            ticket: ticket.clone(),
            after,
        });
        SandboxAction::Store { ticket, effect }
    }

    fn save(&mut self, after: Option<Navigation>) -> SandboxAction {
        if self.live_pending.is_some() {
            self.status = BUSY.into();
            return SandboxAction::None;
        }
        let Some(baseline) = self.formatting.clone().or_else(|| self.baseline.clone()) else {
            self.status = "Load configuration successfully before saving.".into();
            return SandboxAction::None;
        };
        match self.candidate() {
            Ok(draft) => {
                if baseline.saved().configuration().networks != draft.networks {
                    let mut preview = NETWORK_SAVE_NOTICE.to_owned();
                    preview.push_str("\n\nAffected existing instances (last snapshot; worker revalidates after commit):\n");
                    match self.snapshot.as_ref().map(|snapshot| &snapshot.instances) {
                        Some(SnapshotState::Ready(rows)) => {
                            for record in rows.iter().filter_map(|row| row.record.as_ref()) {
                                let Some(launch) = record.launch.as_ref().map(|launch| launch.configuration()) else { continue; };
                                let network = &launch.profile.value().network;
                                if record.ownership != Ownership::Owned || record.detached
                                    || baseline.saved().configuration().networks.get(network) == draft.networks.get(network) { continue; }
                                let current = draft.profiles.get(&launch.profile_name).map(|profile| profile.network.as_str()).unwrap_or("missing");
                                preview.push_str(&format!("{}: launch network {network}; current profile network {current}\n", record.name));
                            }
                        }
                        _ => preview.push_str("Inventory unavailable; affected names will be discovered by the worker.\n"),
                    }
                    self.confirmation = Some(Confirmation::NetworkSave {
                        effect: Box::new(StoreEffect::Save { baseline, draft }),
                        after,
                        preview,
                        draft_revision: self.revision,
                        choice: 0,
                    });
                    self.detail_scroll = 0;
                    return SandboxAction::None;
                }
                self.status = "Saving configuration with file revision precondition…".into();
                self.effect(StoreEffect::Save { baseline, draft }, after)
            }
            Err(error) => {
                self.status = error;
                if let Some(form) = self.form.as_mut() {
                    form.focus_error();
                    self.focus = Focus::Detail;
                    self.detail = true;
                }
                SandboxAction::None
            }
        }
    }

    fn navigate(&mut self, after: Navigation) -> SandboxAction {
        if self.pending.is_some() {
            self.status = BUSY.into();
            return SandboxAction::None;
        }
        if self.dirty() {
            self.confirmation = Some(Confirmation::Dirty { after, choice: 0 });
            self.status = KEEP_EDITING.into();
            SandboxAction::None
        } else {
            self.go(after)
        }
    }

    fn go(&mut self, after: Navigation) -> SandboxAction {
        self.cancel_selections();
        self.instance_action = None;
        self.confirmation = None;
        self.pressed = None;
        self.hits.clear();
        for hint in &mut self.hints {
            hint.reset();
        }
        match after {
            Navigation::Back => {
                if self.document.take().is_none() {
                    if self.detail {
                        self.detail = false;
                        self.focus = Focus::List;
                    } else if self.policies.take().is_some() {
                        self.selected = 0;
                        self.inspect();
                    } else {
                        self.open = false;
                    }
                }
            }
            Navigation::View(view) => {
                self.view = view;
                self.policies = None;
                self.document = None;
                self.search.set_text(String::new());
                self.selected = 0;
                self.list_scroll = 0;
                self.detail = false;
                self.focus = Focus::List;
                self.inspect();
            }
            Navigation::Select(index) => {
                self.selected = index;
                self.inspect();
                self.detail = true;
                self.focus = Focus::Detail;
            }
            Navigation::Policies(kind) => {
                self.view = SandboxView::Profiles;
                self.policies = Some(kind);
                self.document = None;
                self.search.set_text(String::new());
                self.selected = 0;
                self.list_scroll = 0;
                self.detail = false;
                self.focus = Focus::List;
                self.inspect();
            }
            Navigation::New => {
                if let Some(kind) = self.kind().filter(|_| self.baseline.is_some()) {
                    match Form::new(kind, None, None) {
                        Ok(form) => {
                            self.form = Some(form);
                            self.detail = true;
                            self.focus = Focus::Detail;
                            self.revision += 1;
                            self.validate();
                        }
                        Err(error) => self.status = error,
                    }
                }
            }
            Navigation::Duplicate => {
                if let Some(form) = self.form.as_mut() {
                    form.duplicate();
                    self.detail = true;
                    self.focus = Focus::Detail;
                    self.revision += 1;
                }
            }
            Navigation::Import => {
                self.document = Some(DocumentEditor {
                    mode: DocumentMode::Import,
                    editor: TextEditor::new(),
                    destination: None,
                    export: None,
                });
                self.status = IMPORT_HELP.into();
            }
            Navigation::Reload => {
                self.status = "Reloading configuration…".into();
                return self.effect(StoreEffect::Load, None);
            }
        }
        SandboxAction::None
    }

    fn confirm(&mut self, choice: usize) -> SandboxAction {
        match self.confirmation.take() {
            Some(Confirmation::NetworkSave {
                effect,
                after,
                draft_revision,
                ..
            }) if choice == 1 => {
                if self.revision != draft_revision {
                    self.status = "Draft changed after network review. Save again to review the current changes; nothing was persisted.".into();
                    return SandboxAction::None;
                }
                self.status =
                    "Saving configuration; network reconciliation starts only after commit.".into();
                self.effect(*effect, after)
            }
            Some(Confirmation::Live { operation, .. }) if choice == 1 => {
                self.start_operation(*operation)
            }
            Some(Confirmation::Dirty { after, .. }) => match choice {
                1 => self.save(Some(after)),
                2 => {
                    if matches!(after, Navigation::Reload) {
                        return self.go(after);
                    }
                    self.draft = self
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.draft())
                        .unwrap_or_default();
                    self.document = None;
                    self.formatting = None;
                    self.inspect();
                    self.revision += 1;
                    self.go(after)
                }
                _ => {
                    self.status = "Draft retained.".into();
                    SandboxAction::None
                }
            },
            Some(Confirmation::DeleteProfile { name, .. }) if choice == 1 => {
                match self.draft.remove(RecordKind::Profile, &name) {
                    Ok(_) => {
                        self.form = None;
                        self.revision += 1;
                        self.status =
                            "Profile deletion staged; Ctrl+S saves. Instances are never deleted."
                                .into();
                    }
                    Err(error) => self.status = error.to_string(),
                }
                SandboxAction::None
            }
            _ => SandboxAction::None,
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> SandboxAction {
        if event.kind == KeyEventKind::Release {
            return SandboxAction::None;
        }
        let reader = self.reader_focus.or_else(|| {
            ((!matches!(event.code, KeyCode::Left | KeyCode::Right) && self.confirmation.is_some())
                || self.live_pending.is_some()
                || (self.focus == Focus::Detail
                    && self.form.is_none()
                    && self.instance_action.is_none()))
            .then_some(ReadSurface::Body)
        });
        if let Some(surface) = reader.filter(|_| read_only_key(event)) {
            let reader = &mut self.readers[surface as usize];
            if !reader.area.is_empty() {
                let mut event = event;
                if matches!(event.code, KeyCode::Home | KeyCode::End) {
                    event.modifiers.insert(KeyModifiers::CONTROL);
                }
                return editor_action(reader.editor.handle_key(event));
            }
        }
        if self.reader_focus.is_some()
            && !read_only_key(event)
            && !matches!(
                event.code,
                KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab | KeyCode::F(_)
            )
        {
            return SandboxAction::None;
        }
        if !read_only_key(event) {
            self.reader_focus = None;
        }
        if matches!(
            event.code,
            KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab | KeyCode::F(6)
        ) {
            self.cancel_selections();
        }
        self.reset_mouse();
        if self.transfer.is_some() {
            return self.transfer_key(event);
        }
        if event.kind == KeyEventKind::Repeat
            && (self.confirmation.is_some() || !self.form.as_ref().is_some_and(|form| form.editing))
            && !matches!(
                event.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            return SandboxAction::None;
        }
        if self.confirmation.is_some() {
            if matches!(event.code, KeyCode::Esc | KeyCode::Char('k')) {
                return self.confirm(0);
            }
            if event.code == KeyCode::Char('s') {
                return self.confirm(1);
            }
            if event.code == KeyCode::Char('d')
                && matches!(self.confirmation, Some(Confirmation::Dirty { .. }))
            {
                return self.confirm(2);
            }
            if let Some(confirmation) = self.confirmation.as_mut() {
                let (choice, count) = match confirmation {
                    Confirmation::Dirty { choice, .. } => (choice, 3),
                    Confirmation::DeleteProfile { choice, .. }
                    | Confirmation::NetworkSave { choice, .. }
                    | Confirmation::Live { choice, .. } => (choice, 2),
                };
                match event.code {
                    KeyCode::Tab | KeyCode::Right => *choice = (*choice + 1) % count,
                    KeyCode::BackTab | KeyCode::Left => *choice = (*choice + count - 1) % count,
                    KeyCode::Enter => {
                        let choice = *choice;
                        return self.confirm(choice);
                    }
                    KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(5),
                    KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(5),
                    KeyCode::Home => self.detail_scroll = 0,
                    KeyCode::End => self.detail_scroll = u16::MAX,
                    _ => {}
                }
            }
            return SandboxAction::None;
        }
        if self.live_pending.is_some() {
            if event.code == KeyCode::Esc {
                self.open = false;
            } else {
                self.status = "Operation running. Esc closes the manager, NOT the operation. Reconcile its durable outcome; Cancel create is a separate explicit action.".into();
            }
            return SandboxAction::None;
        }
        if self.live_form.is_some() {
            if self
                .live_form
                .as_ref()
                .and_then(|form| form.fields.get(form.focus))
                .is_some_and(|field| field.secret)
                && ((event.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(event.code, KeyCode::Char('a' | 'c' | 'x')))
                    || (event.modifiers.contains(KeyModifiers::SHIFT) && read_only_key(event)))
            {
                return SandboxAction::None;
            }
            return self.live_key(event);
        }
        if self.references.is_some() {
            return self.reference_key(event);
        }
        if self.instance_action.is_some() {
            return self.instance_actions_key(event);
        }
        if self.document.is_some() {
            return self.document_key(event);
        }
        if event.code == KeyCode::F(3) && self.view == SandboxView::Instances {
            self.open_instance_actions();
            return SandboxAction::None;
        }
        if event.code == KeyCode::F(2) {
            self.open_references();
            return SandboxAction::None;
        }
        if key::SAVE.matches(event) {
            return self.save(None);
        }
        if event.code == KeyCode::Esc {
            if self.focus == Focus::Search {
                self.focus = Focus::List;
                return SandboxAction::None;
            }
            return self.navigate(Navigation::Back);
        }
        if matches!(event.code, KeyCode::Tab | KeyCode::BackTab) {
            let reverse =
                event.code == KeyCode::BackTab || event.modifiers.contains(KeyModifiers::SHIFT);
            if self.focus != Focus::Detail {
                self.focus = Focus::Detail;
                self.detail = true;
            } else if let Some(form) = self.form.as_mut() {
                form.editing = false;
                form.reveal_focus = true;
                if reverse {
                    if form.focus == 0 {
                        self.focus = Focus::List;
                    } else {
                        form.focus -= 1;
                    }
                } else if form.focus + 1 < form.fields.len() {
                    form.focus += 1;
                } else {
                    self.focus = Focus::List;
                }
            } else {
                self.focus = Focus::List;
            }
            return SandboxAction::None;
        }
        if self.focus == Focus::Search {
            if event.code == KeyCode::Enter {
                self.focus = Focus::List;
            } else {
                let before = self.search.text();
                let Ok(result) = self
                    .search
                    .handle_key_bounded(event, MAX_SANDBOX_RECORD_BYTES)
                else {
                    self.status = TOO_LARGE.into();
                    return SandboxAction::None;
                };
                if self.search.text() != before {
                    self.selected = 0;
                    self.list_scroll = 0;
                    self.reveal_selected = true;
                    self.inspect();
                }
                return editor_action(result);
            }
            return SandboxAction::None;
        }
        if self.focus == Focus::Detail
            && let Some(form) = self.form.as_mut().filter(|form| form.editing)
        {
            let field = &mut form.fields[form.focus];
            if event.code == KeyCode::Enter
                && (!field.multiline() || event.modifiers.contains(KeyModifiers::CONTROL))
            {
                form.editing = false;
                self.validate();
                return SandboxAction::None;
            }
            if field.locked.is_some() {
                self.status = field.locked.clone().unwrap_or_default();
                return SandboxAction::None;
            }
            let before = field.text();
            let result = if matches!(field.input, form::Input::Domains | form::Input::Cidrs)
                && form::network_list_key(&mut field.editor, event, MAX_SANDBOX_RECORD_BYTES)
            {
                EditorKey::Consumed
            } else {
                match field
                    .editor
                    .handle_key_bounded(event, MAX_SANDBOX_RECORD_BYTES)
                {
                    Ok(result) => result,
                    Err(()) => {
                        self.status = TOO_LARGE.into();
                        return SandboxAction::None;
                    }
                }
            };
            if field.text() != before {
                self.revision += 1;
                self.validate();
            }
            if matches!(result, EditorKey::Passthrough) {
                return self.navigate(Navigation::Back);
            }
            return editor_action(result);
        }
        if event.code == KeyCode::Char('c') && event.modifiers == KeyModifiers::CONTROL {
            return self.navigate(Navigation::Back);
        }
        match event.code {
            KeyCode::Char('v') if self.kind() == Some(RecordKind::Profile) => {
                self.open_live(live::Kind::Create)
            }
            KeyCode::Char('h') => self.open_live(live::Kind::Doctor),
            KeyCode::Char('k') if self.view == SandboxView::Providers => {
                self.open_live(live::Kind::Credential)
            }
            KeyCode::Char('a') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Attach)
            }
            KeyCode::Char('t') if self.view == SandboxView::Instances => {
                self.open_transfer();
                SandboxAction::None
            }
            KeyCode::Char('d') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Detach)
            }
            KeyCode::Char('p') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Pause)
            }
            KeyCode::Char('u') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Resume)
            }
            KeyCode::Char('e') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Extend)
            }
            KeyCode::Delete if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Delete)
            }
            KeyCode::Char('r') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Reconcile)
            }
            KeyCode::Char('f') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::AcknowledgeFailure)
            }
            KeyCode::Char('z') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::CancelCreate)
            }
            KeyCode::Char('g') if self.view == SandboxView::Instances => {
                self.open_live(live::Kind::Network)
            }
            KeyCode::Char('b') if self.view == SandboxView::Images => {
                self.open_live(live::Kind::Build)
            }
            KeyCode::Char('i') if self.view == SandboxView::Images => {
                self.open_live(live::Kind::ImportImage)
            }
            KeyCode::Char('g') if self.view == SandboxView::Images => {
                self.open_live(live::Kind::Gc)
            }
            KeyCode::Char('l') if self.view == SandboxView::Images => {
                self.open_live(live::Kind::InspectImage)
            }
            KeyCode::Char('1') => self.navigate(Navigation::View(SandboxView::Instances)),
            KeyCode::Char('2') => self.navigate(Navigation::View(SandboxView::Profiles)),
            KeyCode::Char('3') => self.navigate(Navigation::View(SandboxView::Images)),
            KeyCode::Char('4') => self.navigate(Navigation::View(SandboxView::Providers)),
            KeyCode::Char('/') => {
                if self.dirty() {
                    self.status = "Save or discard this draft before filtering the list.".into();
                } else {
                    self.focus = Focus::Search;
                }
                SandboxAction::None
            }
            KeyCode::Char('n') if self.kind().is_some() => self.navigate(Navigation::New),
            KeyCode::Char('d') if self.form.is_some() => self.navigate(Navigation::Duplicate),
            KeyCode::Char('g') if self.view == SandboxView::Profiles => {
                self.navigate(Navigation::Policies(RecordKind::Network))
            }
            KeyCode::Char('t') if self.view == SandboxView::Profiles => {
                self.navigate(Navigation::Policies(RecordKind::Transfer))
            }
            KeyCode::Char('i') => self.navigate(Navigation::Import),
            KeyCode::Char('x' | 'a') => {
                self.export_preview();
                SandboxAction::None
            }
            KeyCode::Char('c') => {
                self.compare();
                SandboxAction::None
            }
            KeyCode::Char('r') => self.navigate(Navigation::Reload),
            KeyCode::Delete if self.kind() == Some(RecordKind::Profile) => {
                if self.dirty() {
                    self.status = "Save or discard edits before staging profile deletion.".into();
                } else if let Some(name) = self.form.as_ref().and_then(|form| form.original.clone())
                {
                    self.confirmation = Some(Confirmation::DeleteProfile { name, choice: 0 });
                }
                SandboxAction::None
            }
            KeyCode::Enter if self.focus == Focus::List => {
                self.navigate(Navigation::Select(self.selected))
            }
            KeyCode::Enter => {
                if let Some(form) = self.form.as_mut() {
                    if let Some(reason) = &form.fields[form.focus].locked {
                        self.status = reason.clone();
                    } else {
                        form.editing = true;
                        form.reveal_focus = true;
                        form.fields[form.focus].editor.move_to_end();
                    }
                }
                SandboxAction::None
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => {
                let delta = match event.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -(self.list_area.height.max(1) as isize),
                    KeyCode::PageDown => self.list_area.height.max(1) as isize,
                    KeyCode::Home => -isize::MAX,
                    _ => isize::MAX,
                };
                if self.focus == Focus::List {
                    let index = self
                        .selected
                        .saturating_add_signed(delta)
                        .min(self.entries().len().saturating_sub(1));
                    if self.dirty() {
                        return self.navigate(Navigation::Select(index));
                    }
                    self.selected = index;
                    self.reveal_selected = true;
                    self.inspect();
                } else if let Some(form) = self.form.as_mut() {
                    form.focus = form
                        .focus
                        .saturating_add_signed(delta)
                        .min(form.fields.len().saturating_sub(1));
                    form.reveal_focus = true;
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add_signed(
                        delta.clamp(i16::MIN as isize, i16::MAX as isize) as i16,
                    );
                    if event.code == KeyCode::End {
                        self.detail_scroll = u16::MAX;
                    } else if event.code == KeyCode::Home {
                        self.detail_scroll = 0;
                    }
                }
                SandboxAction::None
            }
            _ => SandboxAction::None,
        }
    }

    fn paste(&mut self, text: &str) {
        if let Some(panel) = self.transfer.as_mut() {
            panel.paste(text);
            return;
        }
        if self.live_pending.is_some() {
            return;
        }
        if let Some(form) = self.live_form.as_mut() {
            if form.picker.is_some() {
                return;
            }
            if form.fields.is_empty() {
                return;
            }
            let field = &mut form.fields[form.focus];
            if field.secret && !text.bytes().all(|byte| byte.is_ascii_graphic()) {
                self.status =
                    "API key must be visible ASCII without whitespace; paste only the key.".into();
                return;
            }
            if !field
                .editor
                .handle_paste_bounded(text, crate::sandbox::MAX_LIVE_PREVIEW_BYTES)
            {
                self.status = TOO_LARGE.into();
            }
            return;
        }
        if let Some(picker) = self.references.as_mut() {
            if !picker
                .search
                .handle_paste_bounded(text, MAX_SANDBOX_RECORD_BYTES)
            {
                self.status = TOO_LARGE.into();
                return;
            }
            picker.selected = 0;
            return;
        }
        let (editor, limit) = if let Some(document) = self.document.as_mut() {
            if let Some(destination) = document.destination.as_mut() {
                (destination, MAX_SANDBOX_RECORD_BYTES)
            } else if document.mode == DocumentMode::Import {
                (&mut document.editor, MAX_SANDBOX_FILE_BYTES)
            } else {
                return;
            }
        } else if self.focus == Focus::Search {
            (&mut self.search, MAX_SANDBOX_RECORD_BYTES)
        } else if let Some(form) = self.form.as_mut().filter(|form| form.editing) {
            let field = &mut form.fields[form.focus];
            if field.locked.is_some() {
                return;
            }
            (&mut field.editor, MAX_SANDBOX_RECORD_BYTES)
        } else {
            return;
        };
        if !editor.handle_paste_bounded(text, limit) {
            self.status = TOO_LARGE.into();
            return;
        }
        if self.focus == Focus::Search && self.document.is_none() {
            self.selected = 0;
            self.list_scroll = 0;
            self.reveal_selected = true;
            self.inspect();
        } else {
            self.revision += 1;
            self.validate();
        }
    }

    fn document_key(&mut self, event: KeyEvent) -> SandboxAction {
        if event.code == KeyCode::Esc {
            if let Some(document) = self.document.as_mut() {
                if document.destination.take().is_some() {
                    self.status = EXPORT_HELP.into();
                    return SandboxAction::None;
                }
                if document.mode != DocumentMode::Import {
                    self.document = None;
                    if let Some(form) = self.report_draft.take() {
                        self.live_form = Some(form);
                        self.status = "Returned to the unchanged draft. No action retried.".into();
                    }
                    return SandboxAction::None;
                }
            }
            return self.navigate(Navigation::Back);
        }
        let Some(document) = self.document.as_mut() else {
            return SandboxAction::None;
        };
        if let Some(destination) = document.destination.as_mut() {
            if event.code == KeyCode::Enter {
                let path = PathBuf::from(destination.text());
                if !path.is_absolute() {
                    self.status =
                        "Choose an absolute client path for a NEW file (never a remote path)."
                            .into();
                    return SandboxAction::None;
                }
                if let Some(draft) = document.export.clone() {
                    let source = document.editor.text();
                    return self.effect(
                        StoreEffect::Export {
                            path,
                            draft,
                            source,
                        },
                        None,
                    );
                }
            }
            return match destination.handle_key_bounded(event, MAX_SANDBOX_RECORD_BYTES) {
                Ok(result) => editor_action(result),
                Err(()) => {
                    self.status = TOO_LARGE.into();
                    SandboxAction::None
                }
            };
        }
        if document.mode == DocumentMode::Export && key::SAVE.matches(event) {
            document.destination = Some(TextEditor::new());
            self.status = "Save as: absolute client path to a new file. Existing files are never replaced. Enter publishes the preview.".into();
            return SandboxAction::None;
        }
        if document.mode == DocumentMode::Import
            && event.code == KeyCode::Enter
            && event.modifiers.contains(KeyModifiers::CONTROL)
        {
            if self.pending.is_some() {
                self.status = BUSY.into();
                return SandboxAction::None;
            }
            match SandboxDraft::import(&document.editor.text()) {
                Ok(draft) => {
                    let formatting = self
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.with_imported_format(&document.editor.text()))
                        .transpose();
                    match formatting {
                        Ok(formatting) => self.formatting = formatting.map(Arc::new),
                        Err(error) => {
                            self.status = error.to_string();
                            return SandboxAction::None;
                        }
                    }
                    self.draft = draft;
                    self.form = None;
                    self.document = None;
                    self.revision += 1;
                    self.status =
                        "Imported into draft only. Ctrl+S saves validated defaults.".into();
                }
                Err(error) => self.status = error.to_string(),
            }
            return SandboxAction::None;
        }
        if document.mode != DocumentMode::Import && !read_only_key(event) {
            return SandboxAction::None;
        }
        let event = if document.mode != DocumentMode::Import
            && matches!(event.code, KeyCode::Home | KeyCode::End)
        {
            KeyEvent {
                modifiers: event.modifiers | KeyModifiers::CONTROL,
                ..event
            }
        } else {
            event
        };
        let before = document.editor.text();
        let Ok(result) = document
            .editor
            .handle_key_bounded(event, MAX_SANDBOX_FILE_BYTES)
        else {
            self.status = TOO_LARGE.into();
            return SandboxAction::None;
        };
        if document.editor.text() != before {
            self.revision += 1;
        }
        editor_action(result)
    }

    fn export_preview(&mut self) {
        match self.candidate().and_then(|draft| {
            self.formatting
                .as_ref()
                .or(self.baseline.as_ref())
                .ok_or_else(|| "Load configuration first".to_owned())?
                .export_draft(&draft)
                .map(|source| (draft, source))
                .map_err(|error| error.to_string())
        }) {
            Ok((draft, source)) => {
                let mut editor = TextEditor::new();
                editor.set_text(source);
                self.document = Some(DocumentEditor {
                    mode: DocumentMode::Export,
                    editor,
                    destination: None,
                    export: Some(draft),
                });
                self.status = EXPORT_HELP.into();
            }
            Err(error) => self.status = error,
        }
    }

    fn compare(&mut self) {
        let source =
            |draft: &SandboxDraft| draft.export().unwrap_or_else(|error| error.to_string());
        let baseline = self
            .baseline
            .as_ref()
            .map(|baseline| source(baseline.saved().configuration()))
            .unwrap_or_default();
        let draft = match self.candidate() {
            Ok(draft) => source(&draft),
            Err(error) => format!(
                "{error}\nUnvalidated field text is retained in the form. Staged document:\n{}",
                source(&self.draft)
            ),
        };
        let disk = match &self.conflict {
            Some(Ok(latest)) => source(latest.saved().configuration()),
            Some(Err(error)) => error.clone(),
            None => "No external conflict loaded; Reload checks the file.".into(),
        };
        let mut editor = TextEditor::new();
        editor.set_text(format!("BASELINE (saved when opened)\n{baseline}\nDRAFT (future defaults)\n{draft}\nDISK (external)\n{disk}"));
        self.document = Some(DocumentEditor {
            mode: DocumentMode::Compare,
            editor,
            destination: None,
            export: None,
        });
        self.status = "Read-only comparison. Esc returns to the unchanged draft.".into();
    }

    fn mouse(&mut self, event: MouseEvent) -> SandboxAction {
        if self.search_capture {
            if event.kind == MouseEventKind::Up(MouseButton::Left) {
                self.search_capture = false;
                self.pressed = None;
            }
            return match self.search.handle_mouse(&event) {
                EditorMouse::Copy(text) => SandboxAction::Copy(text),
                _ => SandboxAction::None,
            };
        }
        if self.editor_capture {
            if event.kind == MouseEventKind::Up(MouseButton::Left) {
                self.editor_capture = false;
                self.pressed = None;
            }
            return self.editor_mouse(event);
        }
        let at = Position::new(event.column, event.row);
        let captured = self.reader_capture.is_some();
        let reader = self.reader_capture.or_else(|| {
            ReadSurface::ALL
                .into_iter()
                .find(|surface| self.readers[*surface as usize].area.contains(at))
        });
        if let Some(surface) = reader {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.reader_capture = Some(surface);
                self.reader_focus = Some(surface);
                self.pressed = None;
                self.hovered = None;
            }
            let action = self.readers[surface as usize].editor.handle_mouse(&event);
            if event.kind == MouseEventKind::Up(MouseButton::Left) {
                self.reader_capture = None;
                self.pressed = None;
            }
            match action {
                EditorMouse::Copy(text) => return SandboxAction::Copy(text),
                EditorMouse::Consumed => return SandboxAction::None,
                EditorMouse::Passthrough if captured => return SandboxAction::None,
                EditorMouse::Passthrough => {}
            }
        } else if event.kind == MouseEventKind::Down(MouseButton::Left) {
            self.reader_focus = None;
        }
        for index in 0..3 {
            let (bar, area) = match index {
                0 => (&mut self.list_bar, self.list_area),
                1 => (&mut self.fields_bar, self.fields_area),
                _ => (&mut self.references_bar, self.references_area),
            };
            if area.is_empty() {
                continue;
            }
            match bar.handle(&event) {
                ScrollbarMouse::Ignored => {}
                ScrollbarMouse::Consumed => {
                    self.pressed = None;
                    return SandboxAction::None;
                }
                ScrollbarMouse::ScrollTo(top) => {
                    self.pressed = None;
                    match index {
                        0 => {
                            self.list_scroll = top as usize;
                            self.reveal_selected = false;
                        }
                        1 => {
                            if self.live_form.is_some() {
                                self.live_scroll = top as usize;
                            } else if let Some(form) = self.form.as_mut() {
                                form.scroll = top as usize;
                                form.reveal_focus = false;
                            }
                        }
                        _ => {
                            self.reference_scroll = top as usize;
                            self.reveal_reference = false;
                        }
                    }
                    return SandboxAction::None;
                }
            }
        }
        if self.confirmation.is_none()
            && let Some(picker) = self
                .live_form
                .as_mut()
                .and_then(|form| form.picker.as_mut())
        {
            let key = picker.mouse(event);
            if let Some(text) = picker.copy.take() {
                return SandboxAction::Copy(text);
            }
            return key.map_or(SandboxAction::None, |code| {
                self.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
            });
        }
        let at = Position::new(event.column, event.row);
        let hit = self
            .hits
            .iter()
            .find(|(area, control)| area.contains(at) && self.control_enabled(control))
            .map(|(_, control)| control.clone())
            .or_else(|| {
                (self.editor_area.contains(at)
                    && self.confirmation.is_none()
                    && self.live_pending.is_none()
                    && self.transfer.is_none())
                .then_some(Control::Editor)
            });
        self.hovered = hit.clone();
        if !self.area.contains(at) || (event.kind == MouseEventKind::Moved && hit.is_none()) {
            self.pressed = None;
        }
        for hint in &mut self.hints {
            if let Some(key) = hint.handle_mouse(event) {
                return self.handle_key(key);
            }
        }
        if let Some(panel) = self.transfer.as_mut() {
            return panel.mouse(event);
        }
        if event.kind == MouseEventKind::Moved {
            if !self.editor_area.is_empty() && self.confirmation.is_none() {
                self.editor_mouse(event);
            }
            if self.focus == Focus::Search {
                self.search.handle_mouse(&event);
            }
            return SandboxAction::None;
        }
        let pressed = self.pressed.take();
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            self.pressed = hit.clone();
        } else if !matches!(event.kind, MouseEventKind::Up(MouseButton::Left)) {
            self.pressed = pressed.clone();
        }
        if self.editor_area.contains(at) && self.confirmation.is_none() {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.editor_capture = true;
            }
            return self.editor_mouse(event);
        }
        if self.focus == Focus::Search && self.confirmation.is_none() && self.list_area.contains(at)
        {
            if event.kind == MouseEventKind::Down(MouseButton::Left) && hit == Some(Control::Search)
            {
                self.search_capture = true;
                self.pressed = None;
            }
            match self.search.handle_mouse(&event) {
                EditorMouse::Copy(text) => return SandboxAction::Copy(text),
                EditorMouse::Consumed => return SandboxAction::None,
                EditorMouse::Passthrough => {}
            }
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.pressed = hit,
            MouseEventKind::Up(MouseButton::Left) => {
                if hit != pressed {
                    return SandboxAction::None;
                }
                self.cancel_selections();
                self.reset_mouse();
                match hit {
                    Some(Control::Reference(index)) => {
                        self.apply_reference(index);
                    }
                    Some(Control::InstanceAction(index)) => {
                        return self.choose_instance_action(index);
                    }
                    Some(Control::InstanceActions) => self.open_instance_actions(),
                    Some(Control::Confirm(choice)) => return self.confirm(choice),
                    _ if self.confirmation.is_some() => {}
                    Some(Control::LiveField(index)) => {
                        if let Some(form) = self.live_form.as_mut() {
                            form.focus = index;
                        }
                    }
                    _ if self.live_form.is_some() || self.live_pending.is_some() => {}
                    Some(Control::View(view)) => return self.navigate(Navigation::View(view)),
                    Some(Control::Row(index)) => return self.navigate(Navigation::Select(index)),
                    Some(Control::Field(index)) => {
                        if let Some(form) = self.form.as_mut() {
                            form.focus = index;
                            form.editing = false;
                            form.reveal_focus = true;
                            self.focus = Focus::Detail;
                        }
                        return self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    }
                    Some(Control::Search) if !self.dirty() => self.focus = Focus::Search,
                    _ => {}
                }
            }
            _ => {}
        }
        SandboxAction::None
    }

    fn editor_mouse(&mut self, event: MouseEvent) -> SandboxAction {
        if let Some(field) = self
            .live_form
            .as_mut()
            .and_then(|form| form.fields.get_mut(form.focus))
            && field.secret
        {
            field.editor.handle_scrollbar_mouse(&event);
            return SandboxAction::None;
        }
        let editor = if let Some(form) = self.live_form.as_mut() {
            form.fields
                .get_mut(form.focus)
                .map(|field| &mut field.editor)
        } else if let Some(picker) = self.references.as_mut() {
            Some(&mut picker.search)
        } else if let Some(document) = self.document.as_mut() {
            Some(
                document
                    .destination
                    .as_mut()
                    .unwrap_or(&mut document.editor),
            )
        } else if let Some(form) = self.form.as_mut().filter(|form| form.editing) {
            Some(&mut form.fields[form.focus].editor)
        } else {
            None
        };
        match editor.map(|editor| editor.handle_mouse(&event)) {
            Some(EditorMouse::Copy(text)) => SandboxAction::Copy(text),
            _ => SandboxAction::None,
        }
    }

    fn cancel_selections(&mut self) {
        self.reset_bars();
        self.search_capture = false;
        self.pressed = None;
        self.reader_focus = None;
        self.reader_capture = None;
        self.editor_capture = false;
        for reader in &mut self.readers {
            reader.editor.cancel_selection();
        }
        self.search.cancel_selection();
        if let Some(document) = self.document.as_mut() {
            document.editor.cancel_selection();
            if let Some(destination) = document.destination.as_mut() {
                destination.cancel_selection();
            }
        }
        if let Some(picker) = self.references.as_mut() {
            picker.search.cancel_selection();
        }
        if let Some(form) = self.form.as_mut() {
            for field in &mut form.fields {
                field.editor.cancel_selection();
            }
        }
        if let Some(form) = self.live_form.as_mut() {
            for field in &mut form.fields {
                field.editor.cancel_selection();
            }
        }
    }

    fn reset_mouse(&mut self) {
        self.reset_bars();
        self.search_capture = false;
        self.editor_capture = false;
        self.hovered = None;
        self.pressed = None;
        self.hits.clear();
        if let Some(picker) = self
            .live_form
            .as_mut()
            .and_then(|form| form.picker.as_mut())
        {
            picker.reset_mouse();
        }
        if let Some(panel) = self.transfer.as_mut() {
            panel.reset_mouse();
        }
        for hint in &mut self.hints {
            hint.reset();
        }
    }

    fn reset_bars(&mut self) {
        self.list_bar = Scrollbar::default();
        self.fields_bar = Scrollbar::default();
        self.references_bar = Scrollbar::default();
    }

    fn control_enabled(&self, control: &Control) -> bool {
        if self.transfer.is_some() || self.live_pending.is_some() {
            return false;
        }
        if self.confirmation.is_some() {
            return matches!(control, Control::Confirm(_));
        }
        if let Some(form) = &self.live_form {
            return form.picker.is_none()
                && matches!(control, Control::LiveField(index) if *index < form.fields.len());
        }
        if self.references.is_some() {
            return matches!(control, Control::Reference(_));
        }
        if self.instance_action.is_some() {
            return matches!(control, Control::InstanceAction(_));
        }
        if self.document.is_some() {
            return false;
        }
        match control {
            Control::View(_) | Control::Row(_) | Control::InstanceActions => true,
            Control::Search => !self.dirty(),
            Control::Field(index) => self
                .form
                .as_ref()
                .and_then(|form| form.fields.get(*index))
                .is_some_and(|field| field.locked.is_none()),
            _ => false,
        }
    }

    fn open_references(&mut self) {
        let Some(form) = &self.form else {
            self.status = "Select a profile field first.".into();
            return;
        };
        if form.kind != RecordKind::Profile
            && !(form.kind == RecordKind::Provider
                && form.fields[form.focus].key == "credential_ref")
        {
            self.status = "Reference choices are available in profile fields; credential references are entered by name only.".into();
            return;
        }
        let key = form.fields[form.focus].key;
        let names = |names: Vec<String>| {
            names
                .into_iter()
                .map(|name| ReferenceChoice {
                    label: name.clone(),
                    fields: vec![(key, name)],
                })
                .collect()
        };
        let choices = match key {
            "credential_ref" => names(
                self.snapshot
                    .as_ref()
                    .map(|snapshot| {
                        snapshot
                            .credentials
                            .iter()
                            .map(ToString::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            "provider" => names(
                self.draft
                    .providers
                    .keys()
                    .map(ToString::to_string)
                    .collect(),
            ),
            "network" => names(
                self.draft
                    .networks
                    .keys()
                    .map(ToString::to_string)
                    .collect(),
            ),
            "transfer" => names(
                self.draft
                    .transfers
                    .keys()
                    .map(ToString::to_string)
                    .collect(),
            ),
            "template" | "template_revision" => {
                let provider = SandboxName::parse(&form.text("provider"))
                    .ok()
                    .and_then(|name| self.provider(&name, &self.draft));
                match provider.map(|provider| &provider.catalog) {
                    Some(SnapshotState::Ready(catalog)) => catalog
                        .entries()
                        .map(|entry| ReferenceChoice {
                            label: format!(
                                "{} @ {} · {:?} · min {} CPU / {} MiB / {} GiB · Workcell {}",
                                entry.id,
                                entry.revision.as_str(),
                                entry.architecture,
                                entry.minimum_resources.cpus,
                                entry.minimum_resources.memory_mib,
                                entry.minimum_resources.disk_gib,
                                entry.workcell_compatible
                            ),
                            fields: vec![
                                ("template", entry.id.to_string()),
                                ("template_revision", entry.revision.as_str().into()),
                            ],
                        })
                        .collect(),
                    _ => {
                        self.status = LOCKED_UNKNOWN.into();
                        return;
                    }
                }
            }
            _ => {
                self.status = "F2 chooses a provider, immutable template, network or transfer reference. Enter edits this field.".into();
                return;
            }
        };
        self.references = Some(ReferencePicker {
            choices,
            search: TextEditor::new(),
            selected: 0,
        });
        self.status =
            "Type to filter references; Enter applies to the draft only. Esc keeps existing text."
                .into();
    }

    fn reference_key(&mut self, event: KeyEvent) -> SandboxAction {
        self.reveal_reference = true;
        if event.code == KeyCode::Esc {
            self.references = None;
            return SandboxAction::None;
        }
        let Some(picker) = self.references.as_mut() else {
            return SandboxAction::None;
        };
        match event.code {
            KeyCode::Enter => {
                if let Some(index) = picker.filtered().get(picker.selected).copied() {
                    self.apply_reference(index);
                }
            }
            KeyCode::Up | KeyCode::BackTab => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                picker.selected =
                    (picker.selected + 1).min(picker.filtered().len().saturating_sub(1))
            }
            KeyCode::Home | KeyCode::End if event.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.selected = if event.code == KeyCode::Home {
                    0
                } else {
                    picker.filtered().len().saturating_sub(1)
                };
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let page = self.area.height.saturating_sub(1).max(1) as usize;
                picker.selected = if event.code == KeyCode::PageUp {
                    picker.selected.saturating_sub(page)
                } else {
                    picker
                        .selected
                        .saturating_add(page)
                        .min(picker.filtered().len().saturating_sub(1))
                };
            }
            _ => {
                let Ok(action) = picker
                    .search
                    .handle_key_bounded(event, MAX_SANDBOX_RECORD_BYTES)
                else {
                    self.status = TOO_LARGE.into();
                    return SandboxAction::None;
                };
                picker.selected = 0;
                return editor_action(action);
            }
        }
        SandboxAction::None
    }

    fn apply_reference(&mut self, index: usize) {
        let Some(picker) = self.references.take() else {
            return;
        };
        let Some(choice) = picker.choices.get(index) else {
            return;
        };
        if let Some(form) = self.form.as_mut() {
            for (key, value) in &choice.fields {
                if let Some(field) = form.fields.iter_mut().find(|field| field.key == *key) {
                    field.editor.handle_key(key::SELECT_ALL.to_key_event());
                    field
                        .editor
                        .handle_paste_bounded(value, MAX_SANDBOX_RECORD_BYTES);
                }
            }
            form.editing = false;
            self.revision += 1;
            self.validate();
        }
    }
}

fn editor_action(action: EditorKey) -> SandboxAction {
    match action {
        EditorKey::Copy(text) => SandboxAction::Copy(text),
        _ => SandboxAction::None,
    }
}

fn read_only_key(event: KeyEvent) -> bool {
    matches!(
        event.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
    ) || (event.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(event.code, KeyCode::Char('a' | 'c')))
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::live::{INSTANCE_ACTIONS, Kind};
    use super::{
        CONFLICT, Confirmation, Control, DocumentMode, Focus, Form, Navigation, SAVED,
        SandboxAction, SandboxManager, SandboxView, StoreEffect, StoreReply, StoreResult,
        StoreTicket, UNAVAILABLE,
    };
    use crate::components::{Overlay, keybindings::key};
    use crate::sandbox::{
        LiveOperation, LiveOutcome, LiveReply, SandboxAttachment, SandboxInstanceSnapshot,
        SandboxInstanceState, SandboxWorkcellState,
    };
    use crate::sandbox::{
        SandboxProviderSnapshot, SandboxSnapshot, SnapshotState, execute_store_effect,
    };
    use caudra_config::sandbox::persistence::SandboxStore;
    use caudra_config::sandbox::{
        Architecture, DomainRule, Enforcement, ProviderCapabilities, RecordKind, ResourceRange,
        SandboxDraft, SandboxName, TemplateCatalog, TemplateEntry, TlsMode,
    };
    use caudra_sandbox::{InstanceRecord, LifecycleAction, Ownership};
    use caudra_storage::id::CaudraId;
    use caudra_storage::workspace_binding::StoredWorkspaceBinding;
    use caudra_workspace::WorkspacePath;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    const CONFIG: &str = r#"
version = 1
[sandbox.providers.local]
kind = "e2b-libvirt"
api_endpoint = "http://127.0.0.1:3000"
proxy_endpoint = "http://127.0.0.1:49983"
credential_ref = "sandbox-api:test"
[sandbox.networks.deny]
enforcement = "required"
tls_mode = "sni-only"
[sandbox.transfers.source]
respect_gitignore = true
initial_seed = "ask"
delete_extraneous = false
exclude = ["**/.env*", "**/.git/**"]
[sandbox.profiles.dev]
provider = "local"
template = "rust"
template_revision = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
cpus = 2
memory_mib = 2048
disk_gib = 20
cwd = "."
network = "deny"
transfer = "source"
persistent = true
running_ttl_seconds = 3600
on_exit = "detach"
"#;
    const PROVIDER: &str = "local";
    const SAVED_TTL: &str = "3600";
    const NEW_TTL: &str = "7200";
    const LEASE: &str = "Lease seconds";
    const NEWER_TTL: &str = "8100";
    const SECRET: &str = "never-export-this-api-token";
    const DIRECTORY_MODE: u32 = 0o700;
    const LIVE_NAME: &str = "managed";
    const LIVE_ERROR: &str = "Outcome unknown; Reconcile before retrying";
    const IMPORT_COMMENT: &str = "# Imported sandbox defaults stay commented";

    #[test_case(false; "successful_probe")]
    #[test_case(true; "failed_probe_retains_draft")]
    fn first_image_import_is_native_and_probe_is_separately_reviewed(failed: bool) {
        let (directory, _, mut manager) = fixture();
        let source = directory.path().join("first image.qcow2");
        let qemu = directory.path().join("qemu-img");
        let mut bytes = vec![0; 1024];
        bytes[..4].copy_from_slice(b"QFI\xfb");
        bytes[4..8].copy_from_slice(&3_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&16_u32.to_be_bytes());
        bytes[24..32].copy_from_slice(&(8_u64 * 1024 * 1024).to_be_bytes());
        fs::write(&source, bytes).unwrap();
        fs::write(&qemu, "#!/bin/sh\nprintf '%s' '{\"format\":\"qcow2\",\"virtual-size\":8388608,\"cluster-size\":65536}'\n").unwrap();
        fs::set_permissions(&qemu, Permissions::from_mode(DIRECTORY_MODE)).unwrap();
        manager
            .state
            .as_mut()
            .unwrap()
            .go(Navigation::View(SandboxView::Images));
        assert!(manager.state.as_ref().unwrap().snapshot.is_none());
        press(&mut manager, KeyCode::Char('i'));
        let form = manager.state.as_mut().unwrap().live_form.as_mut().unwrap();
        assert_eq!(form.field(super::image::PROVIDER), "local");
        for (label, value) in [
            (super::image::SOURCE, source.to_str().unwrap()),
            (super::image::QEMU, qemu.to_str().unwrap()),
            (super::image::HELPER, "/usr/local/bin/e2b-locald"),
            (super::image::DATABASE, "/srv/e2b/database"),
            (super::image::CATALOG, "/srv/e2b/catalog"),
            ("Template ID", "first-image"),
            ("Workcell version", "test"),
            ("Remote workspace feature", "true"),
            ("Reviewed transfer feature", "true"),
        ] {
            form.fields
                .iter_mut()
                .find(|field| field.label == label)
                .unwrap()
                .editor
                .set_text(value.into());
        }
        press(&mut manager, KeyCode::F(4));
        let Some(Confirmation::Live { preview, .. }) =
            &manager.state.as_ref().unwrap().confirmation
        else {
            panic!("missing probe review")
        };
        assert!(preview.contains(qemu.to_str().unwrap()));
        assert!(preview.contains("qemu_img_sha256"));
        let SandboxAction::Live(request) = manager.state.as_mut().unwrap().confirm(1) else {
            panic!("missing approved probe")
        };
        let LiveOperation::ProbeImage(probe) = request.operation else {
            panic!("expected probe only")
        };
        if failed {
            const REPORT_END: &str = "Final probe diagnostic retained";
            let report = format!("{}\n{REPORT_END}", "Probe diagnostic\n".repeat(100));
            let draft = manager
                .state
                .as_ref()
                .unwrap()
                .live_form
                .as_ref()
                .unwrap()
                .fields
                .iter()
                .map(|field| field.editor.text())
                .collect::<Vec<_>>();
            manager.receive_live(LiveReply {
                ticket: request.ticket,
                scope: request.scope,
                result: Err(report.clone()),
            });
            let state = manager.state.as_ref().unwrap();
            assert!(state.live_form.is_none());
            assert!(
                state
                    .document
                    .as_ref()
                    .unwrap()
                    .editor
                    .text()
                    .contains(&report)
            );
            render(&mut manager, 80, 24);
            press(&mut manager, KeyCode::End);
            assert!(render(&mut manager, 80, 24).contains(REPORT_END));
            press(&mut manager, KeyCode::Esc);
            let state = manager.state.as_ref().unwrap();
            assert!(state.document.is_none());
            assert!(state.report_draft.is_none());
            assert!(state.live_pending.is_none());
            assert_eq!(
                state
                    .live_form
                    .as_ref()
                    .unwrap()
                    .fields
                    .iter()
                    .map(|field| field.editor.text())
                    .collect::<Vec<_>>(),
                draft
            );
            return;
        }
        manager.receive_live(LiveReply {
            ticket: request.ticket,
            scope: request.scope,
            result: Ok(LiveOutcome::ImageProbe(probe.execute_approved().unwrap())),
        });
        assert!(manager.state.as_ref().unwrap().document.is_none());
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        let Some(Confirmation::Live {
            operation, preview, ..
        }) = &manager.state.as_ref().unwrap().confirmation
        else {
            panic!(
                "missing import review: {}",
                manager.state.as_ref().unwrap().status
            )
        };
        let LiveOperation::Admin { request, .. } = operation.as_ref() else {
            panic!("expected import")
        };
        let caudra_sandbox::local_admin::AdminOperation::Import(import) = &request.operation else {
            panic!("expected import")
        };
        assert!(import.expected_revision.is_empty());
        assert_eq!(import.manifest.id.as_str(), "first-image");
        assert_eq!(import.manifest.minimum.disk_size_mb, 8);
        assert!(!import.manifest.guest_ca);
        assert!(preview.contains("OFFLINE"));
        assert!(preview.contains("stdin"));
        assert!(preview.contains(qemu.to_str().unwrap()));
    }

    #[test]
    fn oversized_confirmation_is_refused_without_truncating_authority() {
        let (_directory, _, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        let SnapshotState::Ready(rows) = &mut state.snapshot.as_mut().unwrap().instances else {
            panic!("missing rows")
        };
        rows[0].record.as_mut().unwrap().owner_id =
            "x".repeat(crate::sandbox::MAX_LIVE_PREVIEW_BYTES);
        press(&mut manager, KeyCode::Char('a'));
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        assert!(manager.state.as_ref().unwrap().confirmation.is_none());
        assert!(!manager.pending());
        assert!(manager.state.as_ref().unwrap().live_form.is_some());
    }

    #[test]
    fn network_test_is_not_a_live_operation() {
        let (_directory, _, mut manager) = fixture();
        live_instance(&mut manager, false);
        press(&mut manager, KeyCode::Char('g'));
        let form = manager.state.as_mut().unwrap().live_form.as_mut().unwrap();
        for label in [
            "Domains (one per line)",
            "Test destination (bare host or IP)",
        ] {
            form.fields
                .iter_mut()
                .find(|field| field.label == label)
                .unwrap()
                .editor
                .set_text("example.test".into());
        }
        assert!(matches!(
            press(&mut manager, KeyCode::F(4)),
            SandboxAction::None
        ));
        assert!(
            manager
                .state
                .as_ref()
                .unwrap()
                .status
                .contains(crate::sandbox::RULE_TEST_NOTICE)
        );
        assert!(!manager.pending());
        assert!(manager.state.as_ref().unwrap().confirmation.is_none());
    }

    pub(crate) fn live_instance(manager: &mut SandboxManager, borrowed: bool) {
        if manager
            .state
            .as_ref()
            .unwrap()
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.providers.is_empty())
        {
            install_catalog(manager);
        }
        let state = manager.state.as_mut().unwrap();
        let profile = state.draft.profiles.values().next().unwrap();
        let digest = profile.template_revision.as_str();
        let template = serde_json::from_value(json!({"schemaVersion":1,"id":"rust","architecture":"x86_64","machine":"q35",
            "minimum":{"cpuCount":1,"memoryMB":512,"diskSizeMB":1024},"defaults":{"cpuCount":2,"memoryMB":2048,"diskSizeMB":20480},
            "networkTopology":"slirp-enforced","workcell":{"version":"test","sha256":"","protocolVersion":"2026-07-28","transferProtocol":"workcell-reviewed-v1","remoteWorkspace":true,"workspaceSnapshots":true,"reviewedTransfer":true},
            "build":{"recipe":"import","recipeSHA256":"","sourceRevision":""},"revision":digest,"imageSHA256":digest,"warmStart":false,
            "image":{"format":"qcow2","fileSizeBytes":1024,"virtualSizeBytes":21474836480_u64,"clusterSize":65536,"backingPolicy":"standalone"}})).unwrap();
        let instance = serde_json::from_value(json!({"ownerID":"owner","sandboxID":"sandbox","executionID":"execution","revision":1,"state":"running","workspaceGeneration":"generation","expectedWorkcell":null,
            "template":{"id":"rust","revision":digest,"imageIdentity":digest},"resources":{"cpuCount":2,"memoryMB":2048,"diskSizeMB":20480},"networkTopology":"slirp-enforced","persistent":true,"pauseUnclean":false,"leaseDeadline":null,
            "retention":{"pausedDiskMaxAgeSeconds":0,"deadline":null},"egress":{"enforced":true,"revision":"policy","effectiveRevision":"policy","policy":{"mode":"sni-only","domains":[],"cidrs":[]}}})).unwrap();
        let record = InstanceRecord {
            id: CaudraId::generate(),
            name: SandboxName::parse(LIVE_NAME).unwrap(),
            ownership: if borrowed {
                Ownership::Borrowed
            } else {
                Ownership::Owned
            },
            provider_name: profile.provider.clone(),
            provider: state.draft.providers[&profile.provider].clone(),
            owner_id: "owner".into(),
            cwd: WorkspacePath::new(".").unwrap(),
            launch: None,
            template,
            create: None,
            instance: Some(instance),
            lifecycle: None,
            workcell_binding: None,
            detached: false,
        };
        let mut providers = state.snapshot.as_ref().unwrap().providers.clone();
        let provider = providers.get_mut(&profile.provider).unwrap();
        provider.doctor = Some(caudra_sandbox::Doctor {
            discovery: serde_json::from_value(json!({"apiVersion":"1","ownerID":"owner","serverTime":"2026-09-20T12:00:00Z","authentication":"api_key_namespace","templateID":"rust","networkTopology":"slirp-enforced","tlsModes":["sni-only","mitm"],
                "capabilities":{"idempotentCreate":true,"operationLookup":true,"conditionalMutations":true,"explicitCredentials":true,"persistentDisk":true,"memoryPause":false,"egressPolicy":true,"cancelCreate":true,"templateCatalog":true,"conditionalTemplateCreate":true,"warmStart":false,"localTemplateAdmin":true,"httpTemplateAdmin":false,"liveTlsModeChange":false},
                "limits":{"maxLeaseSeconds":3600,"runtimeAdmission":4,"operationJournalEntries":4096,"listPageSize":100,"resources":{"cpuCount":4,"memoryMB":4096,"diskSizeMB":20480},"newKeyMaxAgeSeconds":300,"newKeyFutureSkewSeconds":30},
                "retention":{"pausedDiskMaxAgeSeconds":0,"operationHistorySeconds":86400,"historyStartsAfter":"instance_removed"},"idempotencyKey":"uuidv7","recovery":"query_operation_never_replay_unknown","credentialScope":"sandbox_lifetime","proxyOrigin":"client_configured"})).unwrap(),
            templates: vec![record.template.clone()],
            capabilities: provider.capabilities.clone(),
        });
        let row = SandboxInstanceSnapshot {
            id: LIVE_NAME.into(),
            provider: profile.provider.clone(),
            state: SandboxInstanceState::Running,
            workcell: SandboxWorkcellState::Pending,
            effective: None,
            lease_deadline: None,
            retention_deadline: None,
            blockers: Vec::new(),
            live: record.instance.clone(),
            record: Some(record),
        };
        state.snapshot = Some(SandboxSnapshot {
            sequence: 1,
            instances: SnapshotState::Ready(vec![row]),
            providers,
            failures: BTreeMap::new(),
            credentials: Vec::new(),
        });
        state.go(Navigation::View(SandboxView::Instances));
    }

    #[test_case(KeyCode::Char('a'); "attach")]
    #[test_case(KeyCode::Char('d'); "detach")]
    #[test_case(KeyCode::Char('p'); "pause")]
    #[test_case(KeyCode::Char('u'); "resume")]
    #[test_case(KeyCode::Char('e'); "extend")]
    #[test_case(KeyCode::Delete; "delete")]
    #[test_case(KeyCode::Char('r'); "reconcile")]
    #[test_case(KeyCode::Char('g'); "apply_network")]
    #[test_case(KeyCode::Char('f'); "acknowledge_failure")]
    fn live_actions_require_separate_nondefault_confirmation(code: KeyCode) {
        let (_directory, store, mut manager) = fixture();
        live_instance(&mut manager, false);
        if code == KeyCode::Char('u') {
            let SnapshotState::Ready(rows) = &mut manager
                .state
                .as_mut()
                .unwrap()
                .snapshot
                .as_mut()
                .unwrap()
                .instances
            else {
                panic!("missing instance")
            };
            rows[0].state = SandboxInstanceState::Paused;
            rows[0].live.as_mut().unwrap().state = caudra_sandbox::dto::InstanceState::Paused;
            rows[0].record.as_mut().unwrap().instance = rows[0].live.clone();
        }
        if code == KeyCode::Char('f') {
            let SnapshotState::Ready(rows) = &mut manager
                .state
                .as_mut()
                .unwrap()
                .snapshot
                .as_mut()
                .unwrap()
                .instances
            else {
                panic!("missing instance")
            };
            let record = rows[0].record.as_mut().unwrap();
            record.lifecycle = Some(serde_json::from_value(json!({"action":"pause", "expected":record.instance.as_ref().unwrap().expected(), "lease_seconds":null, "observed_revision":null, "policy":null, "policy_revision":null, "minimum_lease_deadline":null, "allow_equal_revision":false, "failure_acknowledged":false})).unwrap());
        }
        assert!(matches!(press(&mut manager, code), SandboxAction::None));
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        assert!(matches!(
            manager.state.as_ref().unwrap().confirmation,
            Some(Confirmation::Live { choice: 0, .. })
        ));
        if code == KeyCode::Char('f') {
            let Some(Confirmation::Live { preview, .. }) =
                &manager.state.as_ref().unwrap().confirmation
            else {
                panic!("missing review")
            };
            assert!(preview.contains("requested_intent"));
            assert!(preview.contains("last_observed_instance"));
            assert!(preview.contains("Acknowledge FAILURE, not success"));
        }
        assert!(matches!(
            press(&mut manager, KeyCode::Enter),
            SandboxAction::None
        ));
        assert!(!manager.pending());
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        let SandboxAction::Live(request) = press(&mut manager, KeyCode::Char('s')) else {
            panic!("expected live action")
        };
        assert!(manager.pending());
        assert_eq!(
            request.scope.configuration_revision,
            *store.load().unwrap().saved().revision()
        );
        match code {
            KeyCode::Char('a') => {
                assert!(matches!(request.operation, LiveOperation::Attach { .. }))
            }
            KeyCode::Char('r') => {
                assert!(matches!(request.operation, LiveOperation::Reconcile { .. }))
            }
            KeyCode::Char('z') => assert!(matches!(
                request.operation,
                LiveOperation::CancelCreate { .. }
            )),
            KeyCode::Char('f') => assert!(matches!(
                request.operation,
                LiveOperation::AcknowledgeFailure { .. }
            )),
            _ => assert!(matches!(request.operation, LiveOperation::Control { .. })),
        }
    }

    #[test_case(120, 35; "wide_mouse")]
    #[test_case(40, 18; "narrow_mouse")]
    #[test_case(12, 9; "tiny_mouse")]
    fn borrowed_delete_is_detach_and_mouse_uses_reviewed_payload(width: u16, height: u16) {
        let (_directory, _, mut manager) = fixture();
        live_instance(&mut manager, true);
        press(&mut manager, KeyCode::Delete);
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        let state = manager.state.as_ref().unwrap();
        let Some(Confirmation::Live { preview, .. }) = &state.confirmation else {
            panic!("review missing")
        };
        assert!(preview.contains("DETACH"));
        render(&mut manager, width, height);
        let area = manager
            .state
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|(_, control)| *control == Control::Confirm(1))
            .unwrap()
            .0;
        let SandboxAction::Live(request) = click(&mut manager, area) else {
            panic!("mouse approval did not dispatch")
        };
        assert!(matches!(
            request.operation,
            LiveOperation::Control {
                action: LifecycleAction::Delete {
                    destroy_borrowed: false
                },
                ..
            }
        ));
    }

    #[test_case(0; "manager_session")]
    #[test_case(1; "conversation")]
    #[test_case(2; "operation")]
    #[test_case(3; "configuration_epoch")]
    fn live_stale_callbacks_cannot_clear_pending_or_replace_draft(change: usize) {
        let (_directory, _, mut manager) = fixture();
        live_instance(&mut manager, false);
        press(&mut manager, KeyCode::Char('e'));
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        let SandboxAction::Live(request) = press(&mut manager, KeyCode::Char('s')) else {
            panic!("live request missing")
        };
        let mut scope = request.scope.clone();
        let mut ticket = request.ticket.clone();
        match change {
            0 => ticket.session += 1,
            1 => scope.conversation = CaudraId::generate(),
            2 => ticket.operation += 1,
            _ => scope.configuration_epoch += 1,
        }
        assert!(
            manager
                .receive_live(LiveReply {
                    ticket,
                    scope,
                    result: Err(LIVE_ERROR.into())
                })
                .is_none()
        );
        assert!(manager.pending());
        assert!(manager.state.as_ref().unwrap().live_form.is_none());
        manager.receive_live(LiveReply {
            ticket: request.ticket,
            scope: request.scope,
            result: Err(LIVE_ERROR.into()),
        });
        assert!(!manager.pending());
        assert_eq!(manager.state.as_ref().unwrap().status, LIVE_ERROR);
        assert!(manager.state.as_ref().unwrap().live_form.is_none());
    }

    #[test]
    fn close_does_not_cancel_vm_action_or_install_late_attachment() {
        let (_directory, _, mut manager) = fixture();
        live_instance(&mut manager, false);
        press(&mut manager, KeyCode::Char('a'));
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        let SandboxAction::Live(request) = press(&mut manager, KeyCode::Char('s')) else {
            panic!("live request missing")
        };
        assert!(matches!(
            press(&mut manager, KeyCode::Esc),
            SandboxAction::None
        ));
        assert!(!manager.is_open());
        assert!(manager.pending());
        let binding = StoredWorkspaceBinding::local_from_cwd("/tmp");
        let attachment = SandboxAttachment {
            name: SandboxName::parse(LIVE_NAME).unwrap(),
            binding: Box::new(binding),
            runtime: Box::new(()),
        };
        assert!(
            manager
                .receive_live(LiveReply {
                    ticket: request.ticket,
                    scope: request.scope,
                    result: Ok(LiveOutcome::Attachment(attachment))
                })
                .is_none()
        );
        assert!(!manager.pending());
        assert!(!manager.is_open());
    }

    #[test]
    fn escape_unwinds_one_level_per_press_and_closes_only_at_the_root() {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let extend = INSTANCE_ACTIONS
            .iter()
            .position(|(_, kind)| kind.as_ref() == Some(&Kind::Extend))
            .expect("extend action");
        open_instance_action(&mut manager, extend);
        manager.handle_key(key::SELECT_ALL.to_key_event());
        manager.handle_paste(NEW_TTL);
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        assert!(matches!(
            manager.state.as_ref().unwrap().confirmation,
            Some(Confirmation::Live { .. })
        ));
        press(&mut manager, KeyCode::Esc);
        let state = manager.state.as_ref().unwrap();
        assert!(state.confirmation.is_none());
        assert_eq!(state.live_form.as_ref().unwrap().field(LEASE), NEW_TTL);
        press(&mut manager, KeyCode::Esc);
        let state = manager.state.as_ref().unwrap();
        assert!(state.live_form.is_none());
        assert_eq!(state.instance_action, Some(extend));
        press(&mut manager, KeyCode::Esc);
        assert_eq!(manager.state.as_ref().unwrap().instance_action, None);
        assert!(manager.is_open());
        press(&mut manager, KeyCode::Esc);
        assert!(!manager.is_open());
        assert!(!manager.pending());
        let conversation = manager.state.as_ref().unwrap().conversation;
        manager.open(conversation, SandboxView::Instances);
        press(&mut manager, KeyCode::Char('e'));
        let form = manager.state.as_ref().unwrap().live_form.as_ref().unwrap();
        assert_eq!(form.field(LEASE), NEW_TTL);
        assert_eq!(form.origin, None);
        press(&mut manager, KeyCode::Esc);
        assert_eq!(manager.state.as_ref().unwrap().instance_action, None);
        open_instance_action(&mut manager, extend);
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .live_form
                .as_ref()
                .unwrap()
                .field(LEASE),
            NEW_TTL
        );
        press(&mut manager, KeyCode::F(6));
        let state = manager.state.as_ref().unwrap();
        assert!(state.live_form.is_none());
        assert_eq!(state.instance_action, Some(extend));
        press(&mut manager, KeyCode::Esc);
        press(&mut manager, KeyCode::Char('e'));
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .live_form
                .as_ref()
                .unwrap()
                .field(LEASE),
            SAVED_TTL
        );
        assert!(!manager.pending());
    }

    #[test]
    fn retained_draft_is_single_slot_and_never_follows_another_target() {
        const WRONG: &str = "wrong-provider";
        let (_directory, _store, mut manager) = fixture();
        press(&mut manager, KeyCode::Char('h'));
        manager.handle_key(key::SELECT_ALL.to_key_event());
        manager.handle_paste(WRONG);
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .live_form
                .as_ref()
                .unwrap()
                .field(super::image::PROVIDER),
            WRONG
        );
        press(&mut manager, KeyCode::Esc);
        press(&mut manager, KeyCode::Char('4'));
        press(&mut manager, KeyCode::Char('h'));
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .live_form
                .as_ref()
                .unwrap()
                .field(super::image::PROVIDER),
            PROVIDER
        );
        press(&mut manager, KeyCode::Esc);
        press(&mut manager, KeyCode::Char('2'));
        press(&mut manager, KeyCode::Char('h'));
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .live_form
                .as_ref()
                .unwrap()
                .field(super::image::PROVIDER),
            PROVIDER
        );
    }

    fn open_instance_action(manager: &mut SandboxManager, index: usize) {
        press(manager, KeyCode::F(3));
        for _ in 0..index {
            press(manager, KeyCode::Down);
        }
        assert_eq!(manager.state.as_ref().unwrap().instance_action, Some(index));
        press(manager, KeyCode::Enter);
    }

    #[test]
    fn live_editor_focus_masking_and_paste_never_export_credential() {
        let (_directory, _, mut manager) = fixture();
        press(&mut manager, KeyCode::Char('4'));
        press(&mut manager, KeyCode::Char('k'));
        press(&mut manager, KeyCode::Tab);
        let secret = SECRET.repeat(2);
        manager.handle_paste(&secret);
        manager.handle_key(key::SELECT_ALL.to_key_event());
        assert!(matches!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            SandboxAction::None
        ));
        for (width, height) in [(120, 35), (35, 16), (1, 1)] {
            assert!(!render(&mut manager, width, height).contains(&secret));
        }
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        let Some(Confirmation::Live { preview, .. }) =
            &manager.state.as_ref().unwrap().confirmation
        else {
            panic!("credential preview missing")
        };
        assert!(!preview.contains(&secret));
        assert!(preview.contains("sandbox-api:test"));
        press(&mut manager, KeyCode::Esc);
        assert!(manager.state.as_ref().unwrap().live_form.is_some());
        press(&mut manager, KeyCode::Esc);
        assert!(manager.is_open());
        assert!(manager.state.as_ref().unwrap().live_form.is_none());
        press(&mut manager, KeyCode::Char('k'));
        let form = manager
            .state
            .as_ref()
            .unwrap()
            .live_form
            .as_ref()
            .expect("retained credential draft");
        assert_eq!(form.focus, 1);
        assert_eq!(form.fields[1].editor.text(), secret);
        for (width, height) in [(120, 35), (35, 16), (1, 1)] {
            assert!(!render(&mut manager, width, height).contains(&secret));
        }
        press(&mut manager, KeyCode::F(6));
        assert!(manager.state.as_ref().unwrap().live_form.is_none());
        press(&mut manager, KeyCode::Char('k'));
        let form = manager
            .state
            .as_ref()
            .unwrap()
            .live_form
            .as_ref()
            .expect("fresh credential draft");
        assert_eq!(form.focus, 0);
        assert!(form.fields[1].editor.text().is_empty());
    }

    #[test]
    fn imported_comments_survive_export_and_save_without_resolving_refs() {
        let (_directory, store, mut manager) = fixture();
        press(&mut manager, KeyCode::Char('i'));
        manager.handle_paste(&format!("{IMPORT_COMMENT}\n{CONFIG}"));
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        press(&mut manager, KeyCode::Char('x'));
        let source = manager
            .state
            .as_ref()
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .editor
            .text();
        assert!(source.contains(IMPORT_COMMENT));
        assert!(source.contains("sandbox-api:test"));
        press(&mut manager, KeyCode::Esc);
        let (ticket, effect) = effect(manager.handle_key(key::SAVE.to_key_event()));
        manager.receive(StoreReply {
            ticket,
            result: execute_store_effect(Ok(store), effect),
        });
        assert_eq!(manager.state.as_ref().unwrap().status, SAVED);
        let loaded = manager.baseline().unwrap();
        assert!(
            loaded
                .export_draft(&loaded.draft())
                .unwrap()
                .contains(IMPORT_COMMENT)
        );
    }

    fn install_catalog(manager: &mut SandboxManager) {
        let state = manager.state.as_ref().unwrap();
        let profile = state.draft.profiles.values().next().unwrap();
        let provider = state
            .baseline
            .as_ref()
            .unwrap()
            .saved()
            .record(RecordKind::Provider, &profile.provider)
            .unwrap();
        let fixed = |value| ResourceRange {
            min: value,
            max: value,
            step: 1.try_into().unwrap(),
        };
        let catalog = TemplateCatalog::new(vec![TemplateEntry {
            id: profile.template.clone(),
            revision: profile.template_revision.clone(),
            architecture: Architecture::X86_64,
            minimum_resources: profile.resources(),
            workspace_root: "/workspace".into(),
            snapshot_root: "/snapshots".into(),
            workcell_compatible: true,
            network_modes: vec![Enforcement::Required],
            guest_ca: false,
        }])
        .unwrap();
        let provider = SandboxProviderSnapshot {
            capabilities: ProviderCapabilities {
                provider_revision: provider.revision().clone(),
                architecture: Architecture::X86_64,
                cpus: fixed(profile.cpus),
                memory_mib: fixed(profile.memory_mib),
                disk_gib: fixed(profile.disk_gib),
                disk_growth: false,
                persistent: true,
                max_ttl_seconds: profile.running_ttl_seconds,
                network_modes: vec![Enforcement::Required],
                tls_modes: vec![TlsMode::SniOnly],
            },
            catalog: SnapshotState::Ready(catalog),
            doctor: None,
        };
        let request = manager.snapshot_request(state.conversation).unwrap();
        let snapshot = SandboxSnapshot {
            sequence: 1,
            instances: SnapshotState::Loading,
            providers: BTreeMap::from([(profile.provider.clone(), provider)]),
            failures: BTreeMap::new(),
            credentials: Vec::new(),
        };
        assert!(manager.install_snapshot(&request, snapshot));
    }

    pub(crate) fn fixture() -> (TempDir, SandboxStore, SandboxManager) {
        let directory = Builder::new()
            .permissions(Permissions::from_mode(DIRECTORY_MODE))
            .tempdir()
            .unwrap();
        let store = SandboxStore::from_config_dir(directory.path()).unwrap();
        let baseline = store.load().unwrap();
        store
            .save(&baseline, &SandboxDraft::import(CONFIG).unwrap())
            .unwrap();
        let mut manager = SandboxManager::default();
        let (ticket, effect) = effect(manager.open(CaudraId::generate(), SandboxView::Profiles));
        manager.receive(StoreReply {
            ticket,
            result: execute_store_effect(
                Ok(SandboxStore::from_config_dir(directory.path()).unwrap()),
                effect,
            ),
        });
        (directory, store, manager)
    }

    #[test_case(0, false; "cancel_keeps_draft")]
    #[test_case(1, false; "accept_commits_reviewed_network")]
    #[test_case(1, true; "changed_draft_requires_new_review")]
    fn network_save_reviews_owned_launch_references_before_persistence(choice: usize, stale: bool) {
        let (_directory, store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        let baseline = state.baseline.clone().unwrap();
        let (profile_name, profile) = baseline
            .saved()
            .configuration()
            .profiles
            .iter()
            .next()
            .unwrap();
        let snapshot = state.snapshot.as_mut().unwrap();
        let provider = &snapshot.providers[&profile.provider];
        let SnapshotState::Ready(catalog) = &provider.catalog else {
            panic!("catalog fixture");
        };
        let launch = baseline
            .saved()
            .resolve_launch(profile_name, &provider.capabilities, catalog)
            .unwrap();
        let SnapshotState::Ready(rows) = &mut snapshot.instances else {
            panic!("instance fixture");
        };
        rows[0].record.as_mut().unwrap().launch = Some(launch);
        let other = SandboxName::parse("other-network").unwrap();
        state
            .draft
            .networks
            .insert(other.clone(), Default::default());
        state.draft.profiles.get_mut(profile_name).unwrap().network = other;
        state
            .draft
            .networks
            .get_mut(&profile.network)
            .unwrap()
            .enforcement = Enforcement::Required;
        state.form = None;
        state
            .draft
            .networks
            .get_mut(&profile.network)
            .unwrap()
            .domains
            .push(DomainRule::parse("network-review.example").unwrap());
        assert!(matches!(state.save(None), SandboxAction::None));
        let Some(Confirmation::NetworkSave {
            preview,
            choice: initial_choice,
            ..
        }) = &state.confirmation
        else {
            panic!("network review required");
        };
        assert_eq!(*initial_choice, 0);
        assert!(preview.contains(super::NETWORK_SAVE_NOTICE));
        assert!(preview.contains(LIVE_NAME));
        assert!(preview.contains("current profile network other-network"));
        assert_eq!(
            store.load().unwrap().saved().revision(),
            baseline.saved().revision()
        );
        if stale {
            state.revision += 1;
        }
        let action = state.confirm(choice);
        if choice == 1 && !stale {
            assert!(matches!(
                action,
                SandboxAction::Store {
                    effect: StoreEffect::Save { .. },
                    ..
                }
            ));
        } else {
            assert!(matches!(action, SandboxAction::None));
            assert_eq!(
                store.load().unwrap().saved().revision(),
                baseline.saved().revision()
            );
        }
    }

    fn effect(action: SandboxAction) -> (StoreTicket, StoreEffect) {
        match action {
            SandboxAction::Store { ticket, effect } => (ticket, effect),
            _ => panic!("expected persistence effect"),
        }
    }

    pub(super) fn press(manager: &mut SandboxManager, code: KeyCode) -> SandboxAction {
        manager.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn edit(manager: &mut SandboxManager, field: &str, text: &str) {
        let state = manager.state.as_mut().unwrap();
        state.focus = Focus::Detail;
        state.detail = true;
        let form = state.form.as_mut().unwrap();
        form.focus = form
            .fields
            .iter()
            .position(|candidate| candidate.key == field)
            .unwrap();
        form.editing = true;
        manager.handle_key(key::SELECT_ALL.to_key_event());
        manager.handle_paste(text);
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
    }

    fn render(manager: &mut SandboxManager, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn click(manager: &mut SandboxManager, area: Rect) -> SandboxAction {
        let event = |kind| MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        manager.handle_mouse(event(MouseEventKind::Down(MouseButton::Left)));
        manager.handle_mouse(event(MouseEventKind::Up(MouseButton::Left)))
    }

    #[test_case(RecordKind::Profile, "dev"; "all_profile_fields")]
    #[test_case(RecordKind::Provider, "local"; "provider_references")]
    #[test_case(RecordKind::Network, "deny"; "network_policy")]
    #[test_case(RecordKind::Transfer, "source"; "transfer_policy")]
    fn form_roundtrip(kind: RecordKind, name: &str) {
        let draft = SandboxDraft::import(CONFIG).unwrap();
        let name = SandboxName::parse(name).unwrap();
        let record = draft.get(kind.clone(), &name).unwrap();
        let mut form = Form::new(kind, Some(name), Some(record)).unwrap();
        assert_eq!(form.validate(&draft).unwrap(), draft);
        assert!(!form.dirty());
    }

    #[test_case("cpus", "0"; "zero_cpu")]
    #[test_case("memory_mib", "1.5"; "fractional_memory")]
    #[test_case("running_ttl_seconds", "forever"; "invalid_ttl")]
    #[test_case("template_revision", "latest"; "mutable_revision")]
    #[test_case("cwd", "../outside"; "traversal")]
    #[test_case("network", "missing"; "dangling_reference")]
    fn invalid_form_retains_text_and_focuses_error(field: &str, value: &str) {
        let (_directory, store, mut manager) = fixture();
        let baseline = store.load().unwrap().draft();
        edit(&mut manager, field, value);
        assert!(matches!(
            manager.handle_key(key::SAVE.to_key_event()),
            SandboxAction::None
        ));
        let form = manager.state.as_ref().unwrap().form.as_ref().unwrap();
        assert_eq!(form.fields[form.focus].key, field);
        assert_eq!(form.text(field), value);
        assert!(form.fields[form.focus].error.is_some());
        assert_eq!(store.load().unwrap().draft(), baseline);
    }

    #[test_case(40, 24; "narrow")]
    #[test_case(120, 40; "wide")]
    fn keyboard_tab_edit_undo_paste_and_dirty_back(width: u16, height: u16) {
        let (_directory, _store, mut manager) = fixture();
        render(&mut manager, width, height);
        press(&mut manager, KeyCode::Enter);
        edit(&mut manager, "running_ttl_seconds", NEW_TTL);
        let state = manager.state.as_mut().unwrap();
        state.form.as_mut().unwrap().editing = true;
        manager.handle_key(key::UNDO.to_key_event());
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .form
                .as_ref()
                .unwrap()
                .text("running_ttl_seconds"),
            "3600"
        );
        manager.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .form
                .as_ref()
                .unwrap()
                .text("running_ttl_seconds"),
            NEW_TTL
        );
        press(&mut manager, KeyCode::Tab);
        let form = manager.state.as_ref().unwrap().form.as_ref().unwrap();
        assert_eq!(form.fields[form.focus].key, "on_exit");
        assert!(!form.text("running_ttl_seconds").contains('\t'));
        press(&mut manager, KeyCode::Esc);
        assert!(matches!(
            manager.state.as_ref().unwrap().confirmation,
            Some(Confirmation::Dirty { choice: 0, .. })
        ));
        press(&mut manager, KeyCode::Enter);
        assert!(manager.dirty());
        assert!(manager.is_open());
        press(&mut manager, KeyCode::Esc);
        press(&mut manager, KeyCode::Char('d'));
        assert!(!manager.dirty());
        assert!(!manager.state.as_ref().unwrap().detail);
    }

    #[test_case(40; "narrow")]
    #[test_case(120; "wide")]
    fn mouse_inspect_focus_and_clickable_save(width: u16) {
        let (_directory, _store, mut manager) = fixture();
        render(&mut manager, width, 40);
        let row = manager
            .state
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|(_, control)| *control == Control::Row(0))
            .unwrap()
            .0;
        click(&mut manager, row);
        assert!(manager.state.as_ref().unwrap().detail);
        render(&mut manager, width, 40);
        let field = manager
            .state
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|(_, control)| *control == Control::Field(4))
            .unwrap()
            .0;
        click(&mut manager, field);
        assert!(
            manager
                .state
                .as_ref()
                .unwrap()
                .form
                .as_ref()
                .unwrap()
                .editing
        );
        edit(&mut manager, "cpus", "3");
        let rendered = render(&mut manager, width, 40);
        let byte = rendered.find(key::SAVE.label).unwrap();
        let offset = rendered[..byte].chars().count() as u16;
        let (x, y) = (offset % width, offset / width);
        assert!(matches!(
            click(&mut manager, Rect::new(x, y, 1, 1)),
            SandboxAction::Store { .. }
        ));
    }

    #[test_case(120, 40, 30, 20; "wide_to_narrow")]
    #[test_case(30, 20, 120, 40; "narrow_to_wide")]
    #[test_case(120, 40, 8, 4; "tiny_terminal")]
    fn resize_preserves_invalid_draft_and_rejects_old_mouse_press(
        w1: u16,
        h1: u16,
        w2: u16,
        h2: u16,
    ) {
        let (_directory, _store, mut manager) = fixture();
        edit(&mut manager, "cpus", "invalid");
        render(&mut manager, w1, h1);
        manager.state.as_mut().unwrap().pressed = Some(Control::View(SandboxView::Providers));
        render(&mut manager, w2, h2);
        let state = manager.state.as_ref().unwrap();
        assert!(state.pressed.is_none());
        assert_eq!(state.form.as_ref().unwrap().text("cpus"), "invalid");
        assert_eq!(
            state.form.as_ref().unwrap().fields[state.form.as_ref().unwrap().focus].key,
            "cpus"
        );
    }

    #[test_case(false; "save_acknowledgment")]
    #[test_case(true; "edits_race_save")]
    fn save_updates_baseline_without_losing_newer_edits(race: bool) {
        let (directory, store, mut manager) = fixture();
        edit(&mut manager, "running_ttl_seconds", NEW_TTL);
        let (ticket, request) = effect(manager.handle_key(key::SAVE.to_key_event()));
        assert_eq!(
            store.load().unwrap().draft().profiles[&SandboxName::parse("dev").unwrap()]
                .running_ttl_seconds
                .get(),
            3600
        );
        if race {
            edit(&mut manager, "running_ttl_seconds", NEWER_TTL);
        }
        let result = execute_store_effect(
            Ok(SandboxStore::from_config_dir(directory.path()).unwrap()),
            request,
        );
        manager.receive(StoreReply { ticket, result });
        assert_eq!(manager.dirty(), race);
        let state = manager.state.as_ref().unwrap();
        assert_eq!(
            state.form.as_ref().unwrap().text("running_ttl_seconds"),
            if race { NEWER_TTL } else { NEW_TTL }
        );
        assert_eq!(
            state.baseline.as_ref().unwrap().file_revision(),
            store.load().unwrap().file_revision()
        );
        if !race {
            assert_eq!(state.status, SAVED);
        }
    }

    #[test_case(false; "compare_and_save_as")]
    #[test_case(true; "failed_reload_keeps_draft")]
    fn external_conflict_preserves_draft_and_never_overwrites_external(invalid_external: bool) {
        let (directory, store, mut manager) = fixture();
        edit(&mut manager, "running_ttl_seconds", NEW_TTL);
        let (ticket, request) = effect(manager.handle_key(key::SAVE.to_key_event()));
        let external = store.load().unwrap();
        let mut external_draft = external.draft();
        external_draft.transfers.clear();
        external_draft.profiles.clear();
        store.save(&external, &external_draft).unwrap();
        let result = execute_store_effect(
            Ok(SandboxStore::from_config_dir(directory.path()).unwrap()),
            request,
        );
        manager.receive(StoreReply { ticket, result });
        assert_eq!(manager.state.as_ref().unwrap().status, CONFLICT);
        assert!(manager.dirty());
        press(&mut manager, KeyCode::Char('c'));
        let preview = manager
            .state
            .as_ref()
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .editor
            .text();
        assert!(
            preview.contains("BASELINE") && preview.contains("DRAFT") && preview.contains("DISK")
        );
        press(&mut manager, KeyCode::Esc);
        assert!(manager.dirty());
        if invalid_external {
            fs::write(directory.path().join("sandboxes.toml"), "invalid = [").unwrap();
            press(&mut manager, KeyCode::Char('r'));
            let (ticket, request) = effect(press(&mut manager, KeyCode::Char('d')));
            let result = execute_store_effect(
                Ok(SandboxStore::from_config_dir(directory.path()).unwrap()),
                request,
            );
            manager.receive(StoreReply { ticket, result });
            assert_eq!(
                manager
                    .state
                    .as_ref()
                    .unwrap()
                    .form
                    .as_ref()
                    .unwrap()
                    .text("running_ttl_seconds"),
                NEW_TTL
            );
        } else {
            press(&mut manager, KeyCode::Char('a'));
            manager.handle_key(key::SAVE.to_key_event());
            let path = directory.path().join("export.toml");
            manager.handle_paste(path.to_str().unwrap());
            let (ticket, request) = effect(press(&mut manager, KeyCode::Enter));
            let result = execute_store_effect(
                Ok(SandboxStore::from_config_dir(directory.path()).unwrap()),
                request,
            );
            manager.receive(StoreReply { ticket, result });
            assert!(manager.dirty());
            assert_eq!(store.load().unwrap().draft(), external_draft);
            assert!(SandboxDraft::import(&fs::read_to_string(path).unwrap()).is_ok());
        }
    }

    #[test_case(0; "old_operation")]
    #[test_case(1; "old_manager_session")]
    #[test_case(2; "old_draft_revision")]
    fn stale_save_reply_cannot_replace_draft(change: usize) {
        let (_directory, store, mut manager) = fixture();
        edit(&mut manager, "running_ttl_seconds", NEW_TTL);
        let (mut ticket, _) = effect(manager.handle_key(key::SAVE.to_key_event()));
        match change {
            0 => ticket.operation += 1,
            1 => ticket.session += 1,
            _ => ticket.draft_revision += 1,
        }
        manager.receive(StoreReply {
            ticket,
            result: StoreResult::Saved(Arc::new(store.load().unwrap())),
        });
        assert!(manager.pending());
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .form
                .as_ref()
                .unwrap()
                .text("running_ttl_seconds"),
            NEW_TTL
        );
    }

    #[test_case(false; "strict_import")]
    #[test_case(true; "secret_field_refused")]
    fn import_is_inert_and_export_excludes_secrets(secret: bool) {
        let (_directory, store, mut manager) = fixture();
        press(&mut manager, KeyCode::Char('i'));
        let source = if secret {
            CONFIG.replace(
                "kind = \"e2b-libvirt\"",
                &format!("kind = \"e2b-libvirt\"\napi_key = \"{SECRET}\""),
            )
        } else {
            CONFIG.replace("cpus = 2", "cpus = 3")
        };
        manager.handle_paste(&source);
        manager.handle_key(key::SANDBOX_APPLY.to_key_event());
        if secret {
            let state = manager.state.as_ref().unwrap();
            assert!(matches!(
                state.document.as_ref().unwrap().mode,
                DocumentMode::Import
            ));
            assert!(!state.status.contains(SECRET));
            assert!(state.confirmation.is_none());
        } else {
            assert!(manager.dirty());
            press(&mut manager, KeyCode::Char('x'));
            let source = manager
                .state
                .as_ref()
                .unwrap()
                .document
                .as_ref()
                .unwrap()
                .editor
                .text();
            assert!(SandboxDraft::import(&source).is_ok());
            assert!(!source.contains(SECRET));
        }
        assert_eq!(
            store.load().unwrap().draft(),
            SandboxDraft::import(CONFIG).unwrap()
        );
    }

    #[test_case(false; "duplicate_profile")]
    #[test_case(true; "delete_profile_only")]
    fn profile_actions_are_staged_and_never_instance_actions(delete: bool) {
        let (_directory, store, mut manager) = fixture();
        if delete {
            press(&mut manager, KeyCode::Delete);
            press(&mut manager, KeyCode::Enter);
            assert!(!manager.dirty());
            press(&mut manager, KeyCode::Delete);
            press(&mut manager, KeyCode::Char('s'));
            assert!(manager.state.as_ref().unwrap().draft.profiles.is_empty());
        } else {
            press(&mut manager, KeyCode::Char('d'));
            manager.handle_paste("copy");
            manager.handle_key(key::SANDBOX_APPLY.to_key_event());
            assert_eq!(
                manager
                    .state
                    .as_mut()
                    .unwrap()
                    .candidate()
                    .unwrap()
                    .profiles
                    .len(),
                2
            );
        }
        assert!(manager.dirty());
        assert_eq!(store.load().unwrap().draft().profiles.len(), 1);
    }

    #[test_case(SandboxView::Instances; "instances")]
    #[test_case(SandboxView::Images; "images")]
    fn honest_unavailable_views_have_no_fake_rows_or_actions(view: SandboxView) {
        let (_directory, _store, mut manager) = fixture();
        manager.state.as_mut().unwrap().go(Navigation::View(view));
        assert!(manager.state.as_ref().unwrap().entries().is_empty());
        assert_eq!(
            manager.state.as_ref().unwrap().snapshot_status(),
            UNAVAILABLE
        );
        for code in [KeyCode::Enter, KeyCode::Delete, KeyCode::Char('n')] {
            assert!(matches!(press(&mut manager, code), SandboxAction::None));
        }
        assert!(!manager.dirty());
    }

    #[test_case(false; "replacement")]
    #[test_case(true; "reordering")]
    fn accepted_snapshot_cancels_pressed_row_identity(reorder: bool) {
        let (_directory, _, mut manager) = fixture();
        live_instance(&mut manager, false);
        render(&mut manager, 120, 36);
        let state = manager.state.as_ref().unwrap();
        let (area, _) = state
            .hits
            .iter()
            .find(|(_, control)| *control == Control::Row(0))
            .unwrap();
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        let mut snapshot = state.snapshot.as_ref().unwrap().clone();
        snapshot.sequence += 1;
        let SnapshotState::Ready(rows) = &mut snapshot.instances else {
            panic!("missing instance")
        };
        let mut replacement = rows[0].clone();
        replacement.id = "replacement".into();
        if reorder {
            rows.insert(0, replacement);
        } else {
            rows[0] = replacement;
        }
        let request = manager.snapshot_request(state.conversation).unwrap();
        manager.handle_mouse(event);
        assert!(manager.state.as_ref().unwrap().pressed.is_some());
        assert!(manager.install_snapshot(&request, snapshot));
        assert!(manager.state.as_ref().unwrap().pressed.is_none());
        assert!(manager.state.as_ref().unwrap().hovered.is_none());
        let selected = manager.state.as_ref().unwrap().selected;
        render(&mut manager, 120, 36);
        manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..event
        });
        let state = manager.state.as_ref().unwrap();
        assert_eq!(state.selected, selected);
        assert!(state.focus == Focus::List);
    }

    #[test_case(0; "conversation")]
    #[test_case(1; "manager_session")]
    #[test_case(2; "configuration_epoch")]
    fn snapshot_stale_guards(change: usize) {
        let (_directory, _store, mut manager) = fixture();
        let conversation = manager.state.as_ref().unwrap().conversation;
        let mut request = manager.snapshot_request(conversation).unwrap();
        let snapshot = || SandboxSnapshot {
            sequence: 1,
            instances: SnapshotState::Loading,
            providers: BTreeMap::new(),
            failures: BTreeMap::new(),
            credentials: Vec::new(),
        };
        assert!(manager.install_snapshot(&request, snapshot()));
        assert!(!manager.install_snapshot(&request, snapshot()));
        match change {
            0 => request.conversation = CaudraId::generate(),
            1 => request.manager_session += 1,
            _ => request.configuration_epoch += 1,
        }
        let mut newer = snapshot();
        newer.sequence += 1;
        assert!(!manager.install_snapshot(&request, newer));
    }

    #[test_case(KeyCode::Home, 0; "home")]
    #[test_case(KeyCode::End, usize::MAX; "end")]
    fn boundary_keys_navigate_fields_without_editing(code: KeyCode, expected: usize) {
        let (_directory, _store, mut manager) = fixture();
        press(&mut manager, KeyCode::Enter);
        press(&mut manager, code);
        let state = manager.state.as_ref().unwrap();
        let form = state.form.as_ref().unwrap();
        assert_eq!(form.focus, expected.min(form.fields.len() - 1));
        assert!(!form.editing);
        assert!(!manager.dirty());
    }

    #[test_case("domains", "api.example.com"; "domains")]
    #[test_case("cidrs", "10.0.0.0/24"; "cidrs")]
    fn saved_network_shortcuts_validate_and_advance_revision(field_key: &str, value: &str) {
        let (_directory, store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        state.go(Navigation::Policies(RecordKind::Network));
        state.focus = Focus::Detail;
        let form = state.form.as_mut().unwrap();
        form.focus = form
            .fields
            .iter()
            .position(|field| field.key == field_key)
            .unwrap();
        form.editing = true;
        form.fields[form.focus].editor.set_text(value.into());
        let revision = state.revision;
        state.handle_key(KeyEvent::new(KeyCode::Insert, KeyModifiers::ALT));
        assert_eq!(state.revision, revision + 1);
        let form = state.form.as_ref().unwrap();
        assert_eq!(form.text(field_key), format!("{value}\n"));
        assert!(form.fields[form.focus].error.is_none());
        state.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(state.revision, revision + 1);
        state.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT));
        assert_eq!(state.revision, revision + 2);
        let form = state.form.as_ref().unwrap();
        assert!(form.text(field_key).trim().is_empty());
        assert!(form.fields[form.focus].error.is_none());
        assert_eq!(
            store.load().unwrap().draft(),
            SandboxDraft::import(CONFIG).unwrap()
        );
    }

    #[test_case(KeyCode::Home, 0; "first")]
    #[test_case(KeyCode::End, 3; "last")]
    fn live_field_boundaries_preserve_unmodified_cursor_keys(code: KeyCode, expected: usize) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        state.open_live(super::live::Kind::Network);
        state.live_form.as_mut().unwrap().focus = 1;
        state.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(state.live_form.as_ref().unwrap().focus, 1);
        state.handle_key(KeyEvent::new(code, KeyModifiers::ALT));
        assert_eq!(state.live_form.as_ref().unwrap().focus, expected);
    }

    #[test_case(false; "hover_only")]
    #[test_case(true; "click_selects")]
    fn live_field_mouse_selection_is_inert_until_release(click: bool) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        manager
            .state
            .as_mut()
            .unwrap()
            .open_live(super::live::Kind::Network);
        render(&mut manager, 120, 36);
        let state = manager.state.as_mut().unwrap();
        let (area, _) = state
            .hits
            .iter()
            .find(|(_, control)| *control == Control::LiveField(1))
            .unwrap();
        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        state.mouse(event);
        assert!(state.hovered == Some(Control::LiveField(1)));
        assert_eq!(state.live_form.as_ref().unwrap().focus, 0);
        if click {
            state.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                ..event
            });
            assert_eq!(state.live_form.as_ref().unwrap().focus, 0);
            state.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..event
            });
            assert_eq!(state.live_form.as_ref().unwrap().focus, 1);
        }
        assert!(state.confirmation.is_none());
        assert!(state.live_pending.is_none());
    }

    #[test_case(KeyCode::Home; "home")]
    #[test_case(KeyCode::End; "end")]
    fn boundary_keys_preserve_search_and_editor_focus(code: KeyCode) {
        let (_directory, _store, mut manager) = fixture();
        press(&mut manager, KeyCode::Char('/'));
        manager.handle_paste("profile");
        press(&mut manager, code);
        let state = manager.state.as_ref().unwrap();
        assert!(state.focus == Focus::Search);
        assert_eq!(state.search.text(), "profile");
        press(&mut manager, KeyCode::Esc);
        let state = manager.state.as_mut().unwrap();
        state.form = None;
        state.focus = Focus::Detail;
        state.detail_scroll = 10;
        press(&mut manager, KeyCode::End);
        assert_eq!(manager.state.as_ref().unwrap().detail_scroll, u16::MAX);
        press(&mut manager, KeyCode::Home);
        assert_eq!(manager.state.as_ref().unwrap().detail_scroll, 0);
    }

    #[test_case(false; "outside")]
    #[test_case(true; "keyboard_transition")]
    fn hover_is_inert_and_clears_with_stale_press(transition: bool) {
        let (_directory, _store, mut manager) = fixture();
        render(&mut manager, 120, 36);
        let state = manager.state.as_mut().unwrap();
        let hits = state.hits.clone();
        for (area, control) in hits {
            let enabled = state.control_enabled(&control);
            let focus = state.focus == Focus::List;
            assert!(matches!(
                state.mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE
                }),
                SandboxAction::None
            ));
            assert!(state.hovered == enabled.then_some(control));
            assert_eq!(state.focus == Focus::List, focus);
        }
        let (area, _) = state
            .hits
            .iter()
            .find(|(_, control)| *control == Control::View(SandboxView::Images))
            .cloned()
            .unwrap();
        let event = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        state.mouse(event(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.y,
        ));
        if transition {
            state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        } else {
            state.mouse(event(MouseEventKind::Moved, 0, 0));
        }
        assert!(state.hovered.is_none());
        assert!(state.pressed.is_none());
        state.mouse(event(MouseEventKind::Up(MouseButton::Left), area.x, area.y));
        assert_eq!(state.view, SandboxView::Profiles);
    }

    #[test]
    fn wheel_scroll_is_not_undone_by_render() {
        let (_directory, _store, mut manager) = fixture();
        press(&mut manager, KeyCode::Enter);
        render(&mut manager, 120, 18);
        let area = manager.state.as_ref().unwrap().fields_area;
        manager.scroll_at(Position::new(area.x, area.y), -5);
        let scroll = manager
            .state
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .scroll;
        render(&mut manager, 120, 18);
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .form
                .as_ref()
                .unwrap()
                .scroll,
            scroll
        );
        assert!(scroll > 0);
    }

    #[test_case(RecordKind::Network, "domains", "https://example.com:443/path"; "no_url_or_port_rules")]
    #[test_case(RecordKind::Network, "cidrs", "10.0.0.0/99"; "invalid_cidr")]
    #[test_case(RecordKind::Transfer, "exclude", "../secrets"; "no_transfer_traversal")]
    #[test_case(RecordKind::Provider, "credential_ref", SECRET; "no_raw_credentials")]
    #[test_case(RecordKind::Provider, "api_endpoint", "https://user:secret@example.com"; "no_endpoint_credentials")]
    fn strict_policy_and_provider_validation(kind: RecordKind, field: &str, value: &str) {
        let (_directory, store, mut manager) = fixture();
        let navigation = if kind == RecordKind::Provider {
            Navigation::View(SandboxView::Providers)
        } else {
            Navigation::Policies(kind)
        };
        manager.state.as_mut().unwrap().go(navigation);
        edit(&mut manager, field, value);
        assert!(matches!(
            manager.handle_key(key::SAVE.to_key_event()),
            SandboxAction::None
        ));
        let form = manager.state.as_ref().unwrap().form.as_ref().unwrap();
        assert!(form.fields.iter().any(|field| field.error.is_some()));
        assert_eq!(form.text(field), value);
        assert_eq!(
            store.load().unwrap().draft(),
            SandboxDraft::import(CONFIG).unwrap()
        );
    }

    #[test_case(false; "resource_and_network_locks")]
    #[test_case(true; "known_capability_validation")]
    fn authenticated_capabilities_lock_fields_without_silent_corrections(invalid: bool) {
        let (_directory, _store, mut manager) = fixture();
        if invalid {
            edit(&mut manager, "running_ttl_seconds", NEW_TTL);
        }
        install_catalog(&mut manager);
        let state = manager.state.as_ref().unwrap();
        let form = state.form.as_ref().unwrap();
        assert!(
            form.fields
                .iter()
                .find(|field| field.key == "cpus")
                .unwrap()
                .locked
                .is_some()
        );
        if invalid {
            assert_eq!(form.text("running_ttl_seconds"), NEW_TTL);
            assert!(matches!(
                manager.handle_key(key::SAVE.to_key_event()),
                SandboxAction::None
            ));
        } else {
            manager
                .state
                .as_mut()
                .unwrap()
                .go(Navigation::Policies(RecordKind::Network));
            let form = manager.state.as_ref().unwrap().form.as_ref().unwrap();
            for key in ["enforcement", "tls_mode"] {
                assert!(
                    form.fields
                        .iter()
                        .find(|field| field.key == key)
                        .unwrap()
                        .locked
                        .is_some()
                );
            }
        }
    }

    #[test_case("provider"; "provider_reference")]
    #[test_case("network"; "shared_network")]
    #[test_case("transfer"; "shared_transfer")]
    #[test_case("template"; "catalog_revision_pair")]
    fn keyboard_reference_picker_keeps_drafts_and_pins_templates(field: &str) {
        let (_directory, store, mut manager) = fixture();
        install_catalog(&mut manager);
        let form = manager.state.as_mut().unwrap().form.as_mut().unwrap();
        form.focus = form
            .fields
            .iter()
            .position(|candidate| candidate.key == field)
            .unwrap();
        press(&mut manager, KeyCode::F(2));
        manager.handle_paste("not-found");
        assert!(
            manager
                .state
                .as_ref()
                .unwrap()
                .references
                .as_ref()
                .unwrap()
                .filtered()
                .is_empty()
        );
        manager.handle_key(key::SELECT_ALL.to_key_event());
        press(&mut manager, KeyCode::Backspace);
        press(&mut manager, KeyCode::Enter);
        assert!(manager.state.as_ref().unwrap().references.is_none());
        assert_eq!(
            manager.state.as_mut().unwrap().candidate().unwrap(),
            store.load().unwrap().draft()
        );
        assert!(!manager.pending());
    }

    #[test_case(false; "close_after_ack")]
    #[test_case(true; "new_edit_cancels_navigation")]
    fn dirty_save_navigation_waits_for_matching_ack(race: bool) {
        let (directory, _store, mut manager) = fixture();
        edit(&mut manager, "running_ttl_seconds", NEW_TTL);
        press(&mut manager, KeyCode::Esc);
        let (ticket, request) = effect(press(&mut manager, KeyCode::Char('s')));
        assert!(manager.state.as_ref().unwrap().detail);
        if race {
            edit(&mut manager, "running_ttl_seconds", NEWER_TTL);
        }
        manager.receive(StoreReply {
            ticket,
            result: execute_store_effect(
                Ok(SandboxStore::from_config_dir(directory.path()).unwrap()),
                request,
            ),
        });
        assert_eq!(manager.state.as_ref().unwrap().detail, race);
        assert_eq!(manager.dirty(), race);
    }
}
