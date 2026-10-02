use caudra_agent::permissions::editor::{
    ArgumentMode, ArgumentsDraft, AuthorityCatalog, AuthorityChange, ConfirmationRequirement,
    EditableAuthorityDescriptor, EffectivePolicyPreview, GuardDraft, IdentityDraft,
    NormalizedPermissionDraft, PermissionEditPreview, PermissionEditSession,
    PermissionMatchPreview, PermissionRuleDraft, ProjectDraft, ResourceCapability, ResourceDraft,
    ResourcesDraft, SelectorDraft, SelectorMode, SelectorValue, SemanticChange, TemplateSource,
};
use caudra_agent::permissions::pattern_recognition::PatternCandidate;
use caudra_grab::grab_scope;
use caudra_storage::permission_patterns::{
    ArgumentDomain, ArgumentRole, ObservedTuple, PatternDefinition, PatternToken, SlotCombinations,
    SlotId,
};
use caudra_storage::permission_state::{
    PermissionLifetime, PermissionRuleRecord, StructuredPermissionEffect,
};
use caudra_workbench::render::{self, Row};
use caudra_workbench::text_field::{FieldKind, FieldStyles, TextField, TextKey, is_newline_key};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

use super::changes::{
    COMPACT_DETAIL_ROWS, ChangeReview, ChangeView, DETAIL_ROWS, DetailControl, Side, clipped_line,
};
use super::controls::{
    DOMAIN_COUNT, TemplateStructureEdit, domain_for_mode, domain_index, propose_structure,
};
use super::model::{
    FIXED_VALUE, ScopeActivity, ScopeModel, ScopeSource, access_name, effect_name, lifetime_name,
    literal, resource_kind, safe,
};
use super::pattern::{PATTERN_CHIP_ROWS, PatternControl, PatternPanel};
use super::view::ScopeView;
use super::view::{Disclosure, ScopeControl};
use crate::components::field_styles;
use crate::theme::{self, Theme};

const MAX_BUFFER_BYTES: usize = 64 * 1024;
const FIELD_WIDTH: u16 = 20;
const FIELD_GAP: u16 = 1;
const FORM_CHROME_ROWS: u16 = 6;
const CHANGE_CHROME_ROWS: u16 = 3;
const UNPROTECTED_ONLY: &str = "Unprotected only";
const PROTECTION_LABEL: &str = "Protection";
const NEEDS_PREVIEW: &str = "Not validated · Preview before Save";
const EMPTY_TARGETS: &str =
    "No target configured; not unrestricted. Add a target or explicitly choose unrestricted.";
const NO_AUTHORITY: &str =
    "No host-registered editable authority available. Identity cannot be entered as text.";
const DIRTY_CANCEL: &str = "Discard draft? Esc keeps editing; choose Cancel again to discard.";
const STALE_PREVIEW: &str = "Context changed · draft retained; fresh preview required";
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(250);
const PAGE_ROWS: usize = 4;
const CHANGE_ROWS: usize = 4;
const INPUT_ROWS: u16 = 3;
const INPUT_LABEL: &str = "Value: ";
const INPUT_HELP: &str = "Enter applies · Esc cancels\nShift+Enter newline";
const COPY_NOTICE: &str = "Copy creates a separate permission and leaves the source unchanged. Revoke is a separate reviewed action.";
const ANALYSIS_PENDING: &str =
    "Awaiting fresh host analysis · structure not applied; no command is executed";
const TEST_REQUIRES_PREVIEW: &str =
    "Preview a valid draft before testing an example. Nothing is executed.";
const DEFAULT_TEMPLATE_NAME: &str = "Command template";
const TEMPLATE_TARGET_REQUIRED: &str =
    "Add or select a command target with template support first.";
const TEMPLATE_SOURCE_REQUIRED: &str =
    "Supply a concrete source and absolute workdir. Host analysis executes nothing.";
const TEMPLATE_CREATE: &str =
    "Create a literal template from source; host derives argv and roles without execution.";
const WORKDIR_ATTRIBUTE: &str = "workdir";

#[derive(Clone)]
pub(crate) enum EditorLaunch {
    New,
    Edit(Arc<PermissionRuleRecord>),
    Duplicate(Arc<PermissionRuleRecord>),
    Copy {
        source: Arc<PermissionRuleRecord>,
        draft: Option<Box<PermissionRuleDraft>>,
    },
    Discover(Arc<PatternCandidate>),
}

pub(crate) enum EditorEvent {
    Begin(EditorLaunch),
    Preview {
        revision: u64,
        draft: Box<PermissionRuleDraft>,
    },
    Analyze {
        revision: u64,
        target: usize,
        authority_key: String,
        source: TemplateSource,
        proposed: Box<PatternDefinition>,
    },
    Seed {
        revision: u64,
        target: usize,
        authority_key: String,
        source: TemplateSource,
        name: String,
    },
    Test {
        revision: u64,
        test_revision: u64,
        example: EditorTestExample,
    },
    Save {
        revision: u64,
        acknowledged: BTreeSet<ConfirmationRequirement>,
    },
    Cancel,
    /// Text the value field copied or cut, for the clipboard.
    Copy(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EditorTestExample {
    pub(crate) authority_key: String,
    pub(crate) input: Value,
}

struct PendingAnalysis {
    revision: u64,
    target: usize,
    source: TemplateSource,
}

enum TestState {
    Idle,
    Pending,
    Complete(Result<PermissionMatchPreview, String>),
}

pub(crate) struct EditorPreview {
    pub(crate) normalized: Option<NormalizedPermissionDraft>,
    pub(crate) change: AuthorityChange,
    pub(crate) changes: Vec<SemanticChange>,
    pub(crate) confirmations: BTreeSet<ConfirmationRequirement>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Section {
    Rule,
    Targets,
    Arguments,
    Template,
    Changes,
    Scope,
    Test,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Control {
    Section(Section),
    Authority,
    Family,
    Effect,
    Lifetime,
    Project,
    ProjectPath,
    Label,
    Target,
    AddTarget(usize),
    RemoveTarget,
    Unrestricted,
    SelectorMode,
    SelectorValue,
    Access,
    Protection,
    GuardMode(String),
    GuardValue(String),
    AddGuard(String),
    RemoveGuard(String),
    ArgumentMode,
    Input,
    Pointers,
    PreserveInput,
    TemplateName,
    Slot,
    SlotName,
    Domain,
    DomainValue,
    Value(usize),
    AddValue,
    RemoveValue,
    Combinations,
    Tuple,
    TupleValue(SlotId),
    AddTuple,
    RemoveTuple,
    Source,
    Workdir,
    Analyze,
    Preview,
    Save,
    Cancel,
    Confirm,
    Change(usize),
    Requirement(ConfirmationRequirement),
    ChangeDetail(DetailControl),
    Copy,
    TestInput,
    RunTest,
    StructureToken,
    StructureLiteral,
    AddSlot,
    RemoveSlot,
    LinkSlot,
    SelectSlot(SlotId),
    SeenEvidence,
}

struct FieldRow {
    control: Control,
    label: String,
    value: String,
}

#[derive(Clone, PartialEq, Eq)]
struct Hit {
    area: Rect,
    control: Control,
}

pub(crate) struct ScopeEditor {
    draft: PermissionRuleDraft,
    pub(crate) revision: u64,
    catalog: AuthorityCatalog,
    original: Option<ScopeModel>,
    initial: PermissionRuleDraft,
    section: Section,
    focus: Control,
    target: usize,
    slot: usize,
    value: usize,
    tuple: usize,
    editing: Option<Control>,
    field: TextField,
    source: TemplateSource,
    preview: Option<EditorPreview>,
    template_name: String,
    preview_seen: bool,
    confirmed: bool,
    suspended: bool,
    discard: bool,
    status: String,
    scope_view: ScopeView,
    changes_view: ChangeView,
    hits: Vec<Hit>,
    pressed: Option<Hit>,
    last_key: Option<KeyCode>,
    blocked_key: Option<KeyCode>,
    area: Rect,
    offset: usize,
    reveal_focus: bool,
    preview_requested: Option<u64>,
    preview_deadline: Option<(u64, Duration)>,
    reveal_preview: bool,
    copy_reason: Option<String>,
    analysis_pending: Option<PendingAnalysis>,
    test_input: Option<Value>,
    test_revision: u64,
    test_state: TestState,
    structure_token: usize,
    structure_literal: Option<String>,
}

impl ScopeEditor {
    pub(crate) fn from_session(session: &PermissionEditSession) -> Self {
        Self::new(
            session.draft(),
            session.catalog().clone(),
            session
                .original()
                .map(|record| ScopeModel::record(Arc::new(record.clone()))),
        )
    }

    pub(crate) fn receive_backend_preview(
        &mut self,
        revision: u64,
        preview: &PermissionEditPreview,
    ) {
        self.receive_preview(
            revision,
            Ok(EditorPreview {
                normalized: preview.normalized().cloned(),
                change: preview.authority_change().clone(),
                changes: preview.changes().to_vec(),
                confirmations: preview.requirements().clone(),
            }),
        );
    }

    pub(crate) fn new(
        draft: PermissionRuleDraft,
        catalog: AuthorityCatalog,
        original: Option<ScopeModel>,
    ) -> Self {
        Self {
            initial: draft.clone(),
            draft,
            catalog,
            original,
            revision: 0,
            section: Section::Rule,
            focus: Control::Authority,
            target: 0,
            slot: 0,
            value: 0,
            tuple: 0,
            editing: None,
            field: TextField::new(FieldKind::Block).limited_to(MAX_BUFFER_BYTES),
            source: TemplateSource {
                command: String::new(),
                workdir: PathBuf::new(),
            },
            preview: None,
            template_name: DEFAULT_TEMPLATE_NAME.into(),
            preview_seen: false,
            confirmed: false,
            suspended: false,
            discard: false,
            status: NEEDS_PREVIEW.into(),
            scope_view: ScopeView::default(),
            changes_view: ChangeView::default(),
            hits: Vec::new(),
            pressed: None,
            last_key: None,
            blocked_key: None,
            area: Rect::default(),
            offset: 0,
            reveal_focus: true,
            preview_requested: None,
            preview_deadline: None,
            reveal_preview: true,
            copy_reason: None,
            analysis_pending: None,
            test_input: None,
            test_revision: 0,
            test_state: TestState::Idle,
            structure_token: 0,
            structure_literal: None,
        }
    }

    pub(crate) fn draft(&self) -> &PermissionRuleDraft {
        &self.draft
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub(crate) fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    pub(crate) fn set_visible(&mut self, visible: bool) {
        if visible == !self.suspended {
            return;
        }
        if visible {
            self.resume(self.catalog.clone());
        } else {
            self.suspend();
        }
    }

    pub(crate) fn poll_preview(&mut self, now: Duration) -> Option<EditorEvent> {
        if self.suspended || self.editing.is_some() || self.analysis_pending.is_some() {
            self.preview_deadline = None;
            return None;
        }
        if self.preview_requested == Some(self.revision) {
            return None;
        }
        match self.preview_deadline {
            Some((revision, deadline)) if revision == self.revision && now >= deadline => {
                self.request_preview(false)
            }
            Some((revision, _)) if revision == self.revision => None,
            _ => {
                self.preview_deadline = Some((self.revision, now.saturating_add(PREVIEW_DEBOUNCE)));
                None
            }
        }
    }

    fn request_preview(&mut self, reveal: bool) -> Option<EditorEvent> {
        if self.suspended || self.editing.is_some() || self.analysis_pending.is_some() {
            return None;
        }
        self.preview_requested = Some(self.revision);
        self.preview_deadline = None;
        self.reveal_preview = reveal;
        self.blocked_key = self.last_key;
        self.status = "Validating · no policy written".into();
        Some(EditorEvent::Preview {
            revision: self.revision,
            draft: Box::new(self.draft.clone()),
        })
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.draft != self.initial
            || self.editing.is_some()
            || !self.source.command.is_empty()
            || !self.source.workdir.as_os_str().is_empty()
            || self.template_name != DEFAULT_TEMPLATE_NAME
    }

    pub(crate) fn suspend(&mut self) {
        if self.suspended {
            return;
        }
        self.suspended = true;
        self.revision = self.revision.saturating_add(1);
        self.invalidate(STALE_PREVIEW);
    }

    pub(crate) fn resume(&mut self, catalog: AuthorityCatalog) {
        self.catalog = catalog;
        self.suspended = false;
        self.revision = self.revision.saturating_add(1);
        self.invalidate(STALE_PREVIEW);
    }

    pub(crate) fn receive_preview(&mut self, revision: u64, result: Result<EditorPreview, String>) {
        if revision != self.revision || self.suspended {
            return;
        }
        self.preview_seen = false;
        self.preview_requested = Some(revision);
        self.changes_view = ChangeView::default();
        self.confirmed = false;
        self.blocked_key = self.last_key;
        match result {
            Ok(preview) => {
                self.status = format!(
                    "{:?} · {} changes · review Changes before Save",
                    preview.change,
                    preview.changes.len()
                );
                self.preview = Some(preview);
                if self.reveal_preview && self.editing.is_none() {
                    self.section = Section::Changes;
                    self.focus = Control::Section(Section::Changes);
                    self.offset = 0;
                    self.reveal_focus = true;
                }
            }
            Err(error) => {
                self.preview = None;
                self.status = safe(&error);
            }
        }
        self.hits.clear();
        self.pressed = None;
    }

    pub(crate) fn save_failed(&mut self, error: &str) {
        self.invalidate(&safe(error));
    }

    pub(crate) fn offer_copy(&mut self, reason: &str) {
        self.save_failed(reason);
        self.copy_reason = Some(safe(reason));
        self.section = Section::Rule;
        self.focus = Control::Copy;
        self.status = format!("{} · {COPY_NOTICE}", safe(reason));
    }

    pub(crate) fn receive_test(
        &mut self,
        revision: u64,
        test_revision: u64,
        result: Result<PermissionMatchPreview, String>,
    ) {
        if self.suspended
            || revision != self.revision
            || test_revision != self.test_revision
            || !matches!(self.test_state, TestState::Pending)
        {
            return;
        }
        self.test_state = TestState::Complete(result.map_err(|error| safe(&error)));
    }

    pub(crate) fn analysis_failed(&mut self, revision: u64, error: &str) {
        if revision != self.revision || self.suspended {
            return;
        }
        self.analysis_pending = None;
        self.preview_requested = Some(self.revision);
        self.status = safe(error);
    }

    pub(crate) fn set_analyzed_template(
        &mut self,
        revision: u64,
        target: usize,
        definition: PatternDefinition,
        source: TemplateSource,
    ) {
        if revision != self.revision
            || self.suspended
            || !self.analysis_pending.as_ref().is_some_and(|pending| {
                pending.revision == revision && pending.target == target && pending.source == source
            })
        {
            return;
        }
        if let ResourcesDraft::Constrained(resources) = &mut self.draft.resources
            && let Some(resource) = resources.get_mut(target)
        {
            resource.attributes.insert(
                WORKDIR_ATTRIBUTE.into(),
                SelectorDraft::Replace(SelectorValue::Exact(
                    definition.context.effective_workdir.clone(),
                )),
            );
            resource.selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
                definition: Box::new(definition),
                source: Some(source),
            });
            self.changed();
        }
    }

    fn invalidate(&mut self, status: &str) {
        self.preview = None;
        self.changes_view = ChangeView::default();
        self.preview_seen = false;
        self.confirmed = false;
        self.status = status.into();
        self.pressed = None;
        self.hits.clear();
        self.blocked_key = self.last_key;
        self.preview_deadline = None;
        self.analysis_pending = None;
        self.test_state = TestState::Idle;
    }

    fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.discard = false;
        self.copy_reason = None;
        self.invalidate(NEEDS_PREVIEW);
    }

    fn authority(&self) -> Option<&EditableAuthorityDescriptor> {
        let key = match &self.draft.identity {
            IdentityDraft::Registered { key, .. } => key,
            IdentityDraft::Preserve => {
                let rule = self.original.as_ref()?.rule()?;
                return self.catalog.authorities.iter().find(|authority| {
                    authority.source.subject() == &rule.subject
                        && authority.source.executor() == &rule.executor
                        && authority.unavailable.is_none()
                });
            }
        };
        self.catalog
            .authorities
            .iter()
            .find(|authority| &authority.key == key)
    }

    fn resource(&self) -> Option<&ResourceDraft> {
        if let ResourcesDraft::Constrained(resources) = &self.draft.resources {
            resources.get(self.target)
        } else {
            None
        }
    }

    fn resource_mut(&mut self) -> Option<&mut ResourceDraft> {
        if let ResourcesDraft::Constrained(resources) = &mut self.draft.resources {
            resources.get_mut(self.target)
        } else {
            None
        }
    }

    fn capability(&self) -> Option<&ResourceCapability> {
        let resource = self.resource()?;
        self.authority()?
            .resources
            .iter()
            .find(|capability| capability.kind == resource.kind)
    }

    fn template(&self) -> Option<PatternDefinition> {
        match &self.resource()?.selector {
            SelectorDraft::Replace(SelectorValue::CommandTemplate { definition, .. }) => {
                Some(*definition.clone())
            }
            SelectorDraft::Preserve => self
                .original
                .as_ref()?
                .pattern(self.resource()?.original_index?)
                .cloned(),
            _ => None,
        }
    }

    fn edit_template(&mut self, edit: impl FnOnce(&mut PatternDefinition)) {
        let Some(mut definition) = self.template() else {
            self.status = "A host-analyzed template is required.".into();
            return;
        };
        edit(&mut definition);
        if let Some(resource) = self.resource_mut() {
            let source = match &resource.selector {
                SelectorDraft::Replace(SelectorValue::CommandTemplate { source, .. }) => {
                    source.clone()
                }
                _ => None,
            };
            resource.selector = SelectorDraft::Replace(SelectorValue::CommandTemplate {
                definition: Box::new(definition),
                source,
            });
            self.changed();
        }
    }

    fn analyze_structure(&mut self, edit: Option<TemplateStructureEdit>) -> Option<EditorEvent> {
        let Some(authority) = self
            .authority()
            .filter(|authority| authority.unavailable.is_none())
        else {
            self.status = NO_AUTHORITY.into();
            return None;
        };
        let authority_key = authority.key.clone();
        if !self.capability().is_some_and(|capability| {
            capability
                .selectors
                .contains(&SelectorMode::CommandTemplate)
        }) {
            self.status = TEMPLATE_TARGET_REQUIRED.into();
            return None;
        }
        if self.source.command.trim().is_empty() || !self.source.workdir.is_absolute() {
            self.status = TEMPLATE_SOURCE_REQUIRED.into();
            return None;
        }
        let proposed = match (self.template(), edit) {
            (Some(template), Some(edit)) => match propose_structure(&template, edit) {
                Ok(proposed) => Some(proposed),
                Err(reason) => {
                    self.status = reason.into();
                    return None;
                }
            },
            (Some(template), None) => Some(template),
            (None, None) => None,
            (None, Some(_)) => {
                self.status = TEMPLATE_CREATE.into();
                return None;
            }
        };
        self.changed();
        self.analysis_pending = Some(PendingAnalysis {
            revision: self.revision,
            target: self.target,
            source: self.source.clone(),
        });
        self.status = ANALYSIS_PENDING.into();
        Some(match proposed {
            Some(proposed) => EditorEvent::Analyze {
                revision: self.revision,
                target: self.target,
                authority_key,
                source: self.source.clone(),
                proposed: Box::new(proposed),
            },
            None => EditorEvent::Seed {
                revision: self.revision,
                target: self.target,
                authority_key,
                source: self.source.clone(),
                name: self.template_name.clone(),
            },
        })
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<EditorEvent> {
        if self.suspended {
            return None;
        }
        if key.kind == KeyEventKind::Release {
            if self.blocked_key == Some(key.code) {
                self.blocked_key = None;
            }
            self.last_key = None;
            return None;
        }
        if let Some(control) = self.editing.clone() {
            if key.kind == KeyEventKind::Repeat && matches!(key.code, KeyCode::Enter | KeyCode::Esc)
            {
                return None;
            }
            self.last_key = Some(key.code);
            match key.code {
                KeyCode::Esc => {
                    self.editing = None;
                    self.field.clear();
                    self.blocked_key = self.last_key;
                }
                KeyCode::Enter if !is_newline_key(key) => self.commit_field(control),
                _ => {
                    if let TextKey::Copy(text) | TextKey::Cut(text) = self.field.handle_key(key) {
                        return Some(EditorEvent::Copy(text));
                    }
                }
            }
            return None;
        }
        let fresh = key.kind == KeyEventKind::Press && self.blocked_key != Some(key.code);
        if key.kind == KeyEventKind::Repeat {
            self.blocked_key = Some(key.code);
        }
        if self.blocked_key.is_some_and(|blocked| blocked != key.code) {
            self.blocked_key = None;
        }
        self.last_key = Some(key.code);
        if self.section == Section::Scope
            && self.focus == Control::Section(Section::Scope)
            && self.scope_view.handle_key(key)
        {
            return None;
        }
        if self.section == Section::Changes
            && let Control::ChangeDetail(control) = &self.focus
            && let Some(side) = control.side()
            && self.changes_view.handle_key(side, key)
        {
            self.pressed = None;
            return None;
        }
        match key.code {
            KeyCode::Tab => self.move_focus(key.modifiers.contains(KeyModifiers::SHIFT)),
            KeyCode::Down => self.move_focus(false),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(true),
            KeyCode::PageDown => self.scroll_rows(PAGE_ROWS as i32),
            KeyCode::PageUp => self.scroll_rows(-(PAGE_ROWS as i32)),
            KeyCode::Esc if fresh => {
                if self.discard {
                    self.discard = false;
                    self.status = NEEDS_PREVIEW.into();
                } else {
                    return self.activate(Control::Cancel);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') if fresh && key.modifiers.is_empty() => {
                return self.activate(self.focus.clone());
            }
            KeyCode::Char('s') if fresh && key.modifiers == KeyModifiers::CONTROL => {
                return self.activate(Control::Save);
            }
            KeyCode::Char('p') if fresh && key.modifiers == KeyModifiers::CONTROL => {
                return self.activate(Control::Preview);
            }
            _ => {}
        }
        None
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if !self.suspended && self.editing.is_some() {
            self.field.paste(text);
        }
    }

    fn move_focus(&mut self, reverse: bool) {
        let mut controls = [
            Section::Rule,
            Section::Targets,
            Section::Arguments,
            Section::Template,
            Section::Changes,
            Section::Scope,
            Section::Test,
        ]
        .into_iter()
        .map(Control::Section)
        .collect::<Vec<_>>();
        for row in self.rows() {
            if !controls.contains(&row.control) {
                controls.push(row.control);
            }
        }
        if self.section == Section::Changes
            && self.preview.as_ref().is_some_and(|preview| {
                !preview.changes.is_empty() || !preview.confirmations.is_empty()
            })
        {
            controls.extend(
                [
                    DetailControl::Previous,
                    DetailControl::Next,
                    DetailControl::Pane(Side::Before),
                    DetailControl::Pane(Side::After),
                ]
                .into_iter()
                .map(Control::ChangeDetail),
            );
        }
        controls.extend([Control::Preview, Control::Save, Control::Cancel]);
        let index = controls
            .iter()
            .position(|control| *control == self.focus)
            .unwrap_or_default();
        let index = if reverse {
            (index + controls.len() - 1) % controls.len()
        } else {
            (index + 1) % controls.len()
        };
        self.focus = controls[index].clone();
        self.reveal_focus = true;
        self.pressed = None;
    }

    fn scroll_rows(&mut self, delta: i32) {
        self.offset = self.offset.saturating_add_signed(delta as isize);
        self.reveal_focus = false;
        self.pressed = None;
    }

    fn reveal_row(&mut self, selected: Option<usize>, count: usize, height: u16) {
        let height = usize::from(height);
        if self.reveal_focus
            && height > 0
            && let Some(index) = selected
        {
            if index < self.offset {
                self.offset = index;
            } else if index >= self.offset.saturating_add(height) {
                self.offset = index.saturating_add(1).saturating_sub(height);
            }
        }
        self.offset = self.offset.min(count.saturating_sub(height));
    }

    fn activate(&mut self, control: Control) -> Option<EditorEvent> {
        if self.suspended || self.is_editing() {
            return None;
        }
        self.focus = control.clone();
        self.reveal_focus = true;
        self.pressed = None;
        match control {
            Control::Change(_) | Control::Requirement(_) => {}
            Control::ChangeDetail(control) => {
                if let Some((_, review)) = self
                    .change_reviews()
                    .get(self.changes_view.change.unwrap_or_default())
                {
                    self.changes_view
                        .activate(&control, review.predicates.len());
                }
                self.reveal_focus = false;
            }
            Control::Section(section) => {
                self.section = section;
                self.offset = 0;
            }
            Control::Preview => return self.request_preview(true),
            Control::Copy => {
                if let Some(ScopeModel {
                    source: ScopeSource::Record(source),
                    ..
                }) = &self.original
                {
                    self.blocked_key = self.last_key;
                    return Some(EditorEvent::Begin(EditorLaunch::Copy {
                        source: source.clone(),
                        draft: Some(Box::new(self.draft.clone())),
                    }));
                }
                self.status = "Copy requires a stored source; use New for an unowned draft.".into();
            }
            Control::RunTest => {
                if self.preview.is_none() || self.analysis_pending.is_some() {
                    self.status = TEST_REQUIRES_PREVIEW.into();
                    return None;
                }
                let Some(authority) = self.authority() else {
                    self.status = NO_AUTHORITY.into();
                    return None;
                };
                let authority_key = authority.key.clone();
                let Some(input) = self.test_input.clone() else {
                    self.status =
                        "Supply a typed example input first; missing input is not JSON null."
                            .into();
                    return None;
                };
                self.test_revision = self.test_revision.saturating_add(1);
                self.test_state = TestState::Pending;
                self.blocked_key = self.last_key;
                return Some(EditorEvent::Test {
                    revision: self.revision,
                    test_revision: self.test_revision,
                    example: EditorTestExample {
                        authority_key,
                        input,
                    },
                });
            }
            Control::Save => {
                if self.preview_seen
                    && self
                        .preview
                        .as_ref()
                        .is_some_and(|preview| preview.confirmations.is_empty() || self.confirmed)
                {
                    self.blocked_key = self.last_key;
                    self.status = "Saving · awaiting durable acknowledgment".into();
                    self.preview_seen = false;
                    return Some(EditorEvent::Save {
                        revision: self.revision,
                        acknowledged: self
                            .preview
                            .as_ref()
                            .map(|preview| preview.confirmations.clone())
                            .unwrap_or_default(),
                    });
                }
                self.section = Section::Changes;
                self.status = "Save unavailable · Preview, inspect Changes, then confirm required authority changes.".into();
            }
            Control::Confirm if self.preview_seen => {
                self.confirmed = !self.confirmed;
                self.blocked_key = self.last_key;
            }
            Control::Cancel => {
                if !self.is_dirty() || self.discard {
                    return Some(EditorEvent::Cancel);
                }
                self.discard = true;
                self.status = DIRTY_CANCEL.into();
                self.blocked_key = self.last_key;
            }
            Control::Effect => {
                self.draft.effect = match self.draft.effect {
                    StructuredPermissionEffect::Allow => StructuredPermissionEffect::Deny,
                    StructuredPermissionEffect::Deny => StructuredPermissionEffect::Ask,
                    StructuredPermissionEffect::Ask => StructuredPermissionEffect::Allow,
                };
                self.changed();
            }
            Control::Lifetime => {
                self.draft.lifetime = match self.draft.lifetime {
                    PermissionLifetime::Conversation => PermissionLifetime::Project,
                    PermissionLifetime::Project => PermissionLifetime::Global,
                    _ => PermissionLifetime::Conversation,
                };
                self.changed();
            }
            Control::Project => {
                self.draft.project = match self.draft.project {
                    ProjectDraft::Current => ProjectDraft::None,
                    _ => ProjectDraft::Current,
                };
                self.changed();
            }
            Control::Authority => {
                let start = self
                    .authority()
                    .and_then(|selected| {
                        self.catalog
                            .authorities
                            .iter()
                            .position(|authority| authority.key == selected.key)
                    })
                    .map_or(0, |index| index + 1);
                let next = self
                    .catalog
                    .authorities
                    .iter()
                    .cycle()
                    .skip(start)
                    .take(self.catalog.authorities.len())
                    .find(|authority| authority.unavailable.is_none());
                if let Some(authority) = next {
                    self.draft.identity = IdentityDraft::Registered {
                        key: authority.key.clone(),
                        family: None,
                    };
                    self.changed();
                } else {
                    self.status = NO_AUTHORITY.into();
                }
            }
            Control::Family => {
                let families = self
                    .authority()
                    .map(|authority| authority.families.clone())
                    .unwrap_or_default();
                if let IdentityDraft::Registered { family, .. } = &mut self.draft.identity {
                    *family = family
                        .and_then(|current| {
                            families.iter().position(|candidate| *candidate == current)
                        })
                        .map_or_else(
                            || families.first().copied(),
                            |index| families.get(index + 1).copied(),
                        );
                    self.changed();
                } else {
                    self.status =
                        "Choose a registered target before changing capability family.".into();
                }
            }
            Control::Target => {
                if let ResourcesDraft::Constrained(resources) = &self.draft.resources {
                    self.target = (self.target + 1) % resources.len().max(1);
                    self.slot = 0;
                }
            }
            Control::AddTarget(index) => {
                if let Some(capability) = self
                    .authority()
                    .and_then(|authority| authority.resources.get(index))
                    .cloned()
                {
                    if !matches!(self.draft.resources, ResourcesDraft::Constrained(_)) {
                        self.draft.resources = ResourcesDraft::Constrained(Vec::new());
                    }
                    if let ResourcesDraft::Constrained(resources) = &mut self.draft.resources {
                        resources.push(ResourceDraft {
                            original_index: None,
                            kind: capability.kind,
                            selector: SelectorDraft::Unconfigured,
                            access: GuardDraft::Unconfigured,
                            protected: GuardDraft::Unconfigured,
                            attributes: Default::default(),
                        });
                        self.target = resources.len() - 1;
                    }
                    self.changed();
                }
            }
            Control::RemoveTarget => {
                if let ResourcesDraft::Constrained(resources) = &mut self.draft.resources
                    && self.target < resources.len()
                {
                    resources.remove(self.target);
                    self.target = self.target.min(resources.len().saturating_sub(1));
                    if resources.is_empty() {
                        self.draft.resources = ResourcesDraft::Unconfigured;
                    }
                    self.changed();
                }
            }
            Control::Unrestricted => {
                if self
                    .authority()
                    .is_some_and(|authority| authority.unrestricted_resources)
                {
                    self.draft.resources = ResourcesDraft::Unrestricted;
                    self.changed();
                } else {
                    self.status = "Registered target does not offer unrestricted resources.".into();
                }
            }
            Control::SelectorMode | Control::GuardMode(_) => self.cycle_selector(control),
            Control::Access => {
                let Some(capability) = self.capability().cloned() else {
                    self.status = NO_AUTHORITY.into();
                    return None;
                };
                if let Some(resource) = self.resource_mut() {
                    let index = if let GuardDraft::Equals(value) = &resource.access {
                        capability
                            .access
                            .iter()
                            .position(|candidate| candidate == value)
                            .map(|index| index + 1)
                    } else {
                        Some(0)
                    };
                    resource.access = index
                        .and_then(|index| capability.access.get(index))
                        .cloned()
                        .map_or(
                            if capability.wildcard_access {
                                GuardDraft::Any
                            } else {
                                GuardDraft::Unconfigured
                            },
                            GuardDraft::Equals,
                        );
                    self.changed();
                }
            }
            Control::Protection => {
                let any = self
                    .capability()
                    .is_some_and(|capability| capability.wildcard_protection);
                if let Some(resource) = self.resource_mut() {
                    resource.protected = match resource.protected {
                        GuardDraft::Equals(false) => GuardDraft::Equals(true),
                        GuardDraft::Equals(true) if any => GuardDraft::Any,
                        _ => GuardDraft::Equals(false),
                    };
                    self.changed();
                }
            }
            Control::AddGuard(name) => {
                if let Some(resource) = self.resource_mut() {
                    resource
                        .attributes
                        .insert(name, SelectorDraft::Unconfigured);
                    self.changed();
                }
            }
            Control::RemoveGuard(name) => {
                if let Some(resource) = self.resource_mut() {
                    resource.attributes.remove(&name);
                    self.changed();
                }
            }
            Control::ArgumentMode => {
                let modes = self
                    .authority()
                    .map(|authority| authority.arguments.clone())
                    .unwrap_or_default();
                let current = match self.draft.arguments {
                    ArgumentsDraft::Exact(_) => Some(ArgumentMode::Exact),
                    ArgumentsDraft::Selected { .. } => Some(ArgumentMode::Selected),
                    ArgumentsDraft::Unconstrained => Some(ArgumentMode::Unconstrained),
                    _ => None,
                };
                let index = current
                    .and_then(|current| modes.iter().position(|mode| *mode == current))
                    .map_or(0, |index| (index + 1) % modes.len().max(1));
                let Some(mode) = modes.get(index) else {
                    self.status =
                        "Input modes are read-only without a currently registered editable target."
                            .into();
                    return None;
                };
                self.draft.arguments = match mode {
                    ArgumentMode::Selected => ArgumentsDraft::Selected {
                        input: Value::Object(Default::default()),
                        pointers: Vec::new(),
                    },
                    ArgumentMode::Unconstrained => ArgumentsDraft::Unconstrained,
                    ArgumentMode::Exact => ArgumentsDraft::Exact(Value::Object(Default::default())),
                };
                self.changed();
            }
            Control::PreserveInput => {
                self.draft.arguments = ArgumentsDraft::PreserveCoupled;
                self.changed();
            }
            Control::Slot => {
                if let Some(template) = self.template() {
                    self.slot = (self.slot + 1) % template.slots.len().max(1);
                    self.value = 0;
                }
            }
            Control::Domain => {
                let index = self.slot;
                self.edit_template(|template| {
                    if let Some(slot) = template.slots.get_mut(index) {
                        let mode = (domain_index(&slot.domain) + 1) % DOMAIN_COUNT;
                        let values = if let ArgumentDomain::ObservedSet { values } = &slot.domain {
                            values.clone()
                        } else {
                            BTreeSet::new()
                        };
                        slot.domain = domain_for_mode(mode, values, None);
                    }
                });
            }
            Control::AddValue => {
                self.editing = Some(Control::AddValue);
                self.field.clear();
            }
            Control::RemoveValue => {
                let (slot, value) = (self.slot, self.value);
                self.edit_template(|template| {
                    if let Some(slot) = template.slots.get_mut(slot)
                        && let ArgumentDomain::ObservedSet { values } = &mut slot.domain
                        && let Some(value) = values.iter().nth(value).cloned()
                    {
                        values.remove(&value);
                    }
                });
            }
            Control::Combinations => self.edit_template(|template| {
                template.combinations = match template.combinations {
                    SlotCombinations::Independent => SlotCombinations::ObservedTuples {
                        tuples: BTreeSet::new(),
                    },
                    _ => SlotCombinations::Independent,
                }
            }),
            Control::Tuple => {
                if let Some(template) = self.template()
                    && let SlotCombinations::ObservedTuples { tuples } = template.combinations
                {
                    self.tuple = (self.tuple + 1) % tuples.len().max(1);
                }
            }
            Control::AddTuple => {
                let mut selected = None;
                self.edit_template(|template| {
                    if let SlotCombinations::ObservedTuples { tuples } = &mut template.combinations
                    {
                        let tuple: ObservedTuple = template
                            .slots
                            .iter()
                            .map(|slot| (slot.id, String::new()))
                            .collect();
                        tuples.insert(tuple.clone());
                        selected = tuples.iter().position(|candidate| candidate == &tuple);
                    }
                });
                if let Some(index) = selected {
                    self.tuple = index;
                }
            }
            Control::RemoveTuple => {
                let index = self.tuple;
                self.edit_template(|template| {
                    if let SlotCombinations::ObservedTuples { tuples } = &mut template.combinations
                        && let Some(tuple) = tuples.iter().nth(index).cloned()
                    {
                        tuples.remove(&tuple);
                    }
                });
                if let Some(template) = self.template()
                    && let SlotCombinations::ObservedTuples { tuples } = template.combinations
                {
                    self.tuple = self.tuple.min(tuples.len().saturating_sub(1));
                }
            }
            Control::Analyze => return self.analyze_structure(None),
            Control::StructureToken => {
                if let Some(template) = self.template() {
                    self.structure_token = (self.structure_token + 1) % template.argv.len().max(1);
                }
            }
            Control::SelectSlot(id) => {
                if let Some(slot) = self
                    .template()
                    .and_then(|template| template.slots.iter().position(|slot| slot.id == id))
                {
                    self.slot = slot;
                    self.value = 0;
                }
            }
            Control::SeenEvidence => {
                self.scope_view.disclosure = Some(Disclosure::Evidence);
                self.section = Section::Scope;
                self.focus = Control::Section(Section::Scope);
            }
            Control::AddSlot => {
                return self.analyze_structure(Some(TemplateStructureEdit::Add {
                    token: self.structure_token,
                }));
            }
            Control::LinkSlot | Control::RemoveSlot => {
                let Some(id) = self
                    .template()
                    .and_then(|template| template.slots.get(self.slot).map(|slot| slot.id))
                else {
                    self.status = "Select an existing slot first.".into();
                    return None;
                };
                let edit = if control == Control::LinkSlot {
                    TemplateStructureEdit::Link {
                        token: self.structure_token,
                        id,
                    }
                } else {
                    let Some(literal) = self.structure_literal.clone() else {
                        self.status = "Supply an explicit fixed replacement, including an explicit empty value if intended.".into();
                        return None;
                    };
                    TemplateStructureEdit::Remove { id, literal }
                };
                return self.analyze_structure(Some(edit));
            }
            control => {
                let text = self.field_text(&control);
                if let Some(text) = text {
                    if let Control::Value(index) = control {
                        self.value = index;
                    }
                    self.field.set_text(&text);
                    self.editing = Some(control);
                } else {
                    self.status = "Read-only in this mode. A fixed value Caudra can't show needs an explicit replacement; values Caudra derives need analysis.".into();
                }
            }
        }
        None
    }

    fn cycle_selector(&mut self, control: Control) {
        let Some(capability) = self.capability() else {
            self.status = "Select a registered target to see supported match modes. Fixed values stay unchanged.".into();
            return;
        };
        let mut modes = match &control {
            Control::GuardMode(name) => {
                capability.attributes.get(name).cloned().unwrap_or_default()
            }
            _ => capability.selectors.clone(),
        };
        modes.retain(|mode| *mode != SelectorMode::CommandTemplate);
        let current = self
            .resource()
            .and_then(|resource| match &control {
                Control::GuardMode(name) => resource.attributes.get(name),
                _ => Some(&resource.selector),
            })
            .and_then(|selector| {
                if let SelectorDraft::Replace(value) = selector {
                    Some(value.mode())
                } else {
                    None
                }
            });
        let index = current
            .and_then(|current| modes.iter().position(|mode| *mode == current))
            .map_or(0, |index| (index + 1) % modes.len().max(1));
        let Some(mode) = modes.get(index) else {
            self.status = "This field has no supported editable selectors.".into();
            return;
        };
        let value = match mode {
            SelectorMode::Exact => SelectorValue::Exact(String::new()),
            SelectorMode::FilesystemSubtree => SelectorValue::FilesystemSubtree(String::new()),
            SelectorMode::UrlSubtree => SelectorValue::UrlSubtree(String::new()),
            SelectorMode::UrlOrigin => SelectorValue::UrlOrigin(String::new()),
            SelectorMode::CommandPattern => SelectorValue::CommandPattern(String::new()),
            SelectorMode::RemoteExact => SelectorValue::RemoteExact(Vec::new()),
            SelectorMode::RemoteSubtree => SelectorValue::RemoteSubtree(Vec::new()),
            SelectorMode::Any => SelectorValue::Any,
            SelectorMode::CommandTemplate => {
                self.section = Section::Template;
                self.focus = Control::TemplateName;
                self.offset = 0;
                return;
            }
        };
        if let Some(resource) = self.resource_mut() {
            match control {
                Control::GuardMode(name) => {
                    resource
                        .attributes
                        .insert(name, SelectorDraft::Replace(value));
                }
                _ => resource.selector = SelectorDraft::Replace(value),
            }
            self.changed();
        }
    }

    fn field_text(&self, control: &Control) -> Option<String> {
        match control {
            Control::TestInput => Some(
                self.test_input
                    .as_ref()
                    .map_or_else(String::new, Value::to_string),
            ),
            Control::StructureLiteral => Some(self.structure_literal.clone().unwrap_or_default()),
            Control::Label => Some(self.draft.label.clone().unwrap_or_default()),
            Control::ProjectPath => {
                Some(if let ProjectDraft::Explicit(path) = &self.draft.project {
                    path.to_string_lossy().into_owned()
                } else {
                    String::new()
                })
            }
            Control::SelectorValue | Control::GuardValue(_) => {
                let resource = self.resource()?;
                let selector = match control {
                    Control::GuardValue(name) => resource.attributes.get(name)?,
                    _ => &resource.selector,
                };
                match selector {
                    SelectorDraft::Replace(value) => selector_edit_text(value),
                    _ => None,
                }
            }
            Control::Input => Some(match &self.draft.arguments {
                ArgumentsDraft::Exact(value) | ArgumentsDraft::Selected { input: value, .. } => {
                    value.to_string()
                }
                _ => return None,
            }),
            Control::Pointers => Some(
                if let ArgumentsDraft::Selected { pointers, .. } = &self.draft.arguments {
                    pointers.join("\n")
                } else {
                    String::new()
                },
            ),
            Control::Source => Some(self.source.command.clone()),
            Control::Workdir => Some(self.source.workdir.to_string_lossy().into_owned()),
            Control::TemplateName => Some(
                self.template()
                    .map_or_else(|| self.template_name.clone(), |template| template.name),
            ),
            Control::SlotName => Some(self.template()?.slots.get(self.slot)?.label.clone()),
            Control::DomainValue => match &self.template()?.slots.get(self.slot)?.domain {
                ArgumentDomain::Exact { value } => Some(value.clone()),
                ArgumentDomain::Glob { pattern } | ArgumentDomain::Regex { pattern } => {
                    Some(pattern.clone())
                }
                _ => None,
            },
            Control::Value(index) => {
                if let ArgumentDomain::ObservedSet { values } =
                    &self.template()?.slots.get(self.slot)?.domain
                {
                    values.iter().nth(*index).cloned()
                } else {
                    None
                }
            }
            Control::TupleValue(id) => {
                if let SlotCombinations::ObservedTuples { tuples } = &self.template()?.combinations
                {
                    tuples.iter().nth(self.tuple)?.get(id).cloned()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn commit_field(&mut self, control: Control) {
        let text = self.field.text();
        if control == Control::TestInput {
            match serde_json::from_str(&text) {
                Ok(value) => self.test_input = Some(value),
                Err(error) => {
                    self.status = format!("JSON value required: {error}");
                    return;
                }
            }
            self.test_revision = self.test_revision.saturating_add(1);
            self.test_state = TestState::Idle;
            self.editing = None;
            self.field.clear();
            self.blocked_key = self.last_key;
            return;
        }
        match control.clone() {
            Control::StructureLiteral => self.structure_literal = Some(text),
            Control::Label => self.draft.label = (!text.is_empty()).then_some(text),
            Control::ProjectPath => self.draft.project = ProjectDraft::Explicit(text.into()),
            Control::SelectorValue | Control::GuardValue(_) => {
                if let Some(resource) = self.resource_mut() {
                    let selector = match &control {
                        Control::GuardValue(name) => resource.attributes.get_mut(name),
                        _ => Some(&mut resource.selector),
                    };
                    if let Some(SelectorDraft::Replace(value)) = selector {
                        replace_selector_text(value, text);
                    }
                }
            }
            Control::Input => match serde_json::from_str(&text) {
                Ok(value) => match &mut self.draft.arguments {
                    ArgumentsDraft::Selected { input, .. } => *input = value,
                    _ => self.draft.arguments = ArgumentsDraft::Exact(value),
                },
                Err(error) => {
                    self.status = format!("JSON value required: {error}");
                    return;
                }
            },
            Control::Pointers => {
                if let ArgumentsDraft::Selected { pointers, .. } = &mut self.draft.arguments {
                    *pointers = text.lines().map(str::to_owned).collect();
                }
            }
            Control::Source => self.source.command = text,
            Control::Workdir => self.source.workdir = text.into(),
            Control::TemplateName if self.template().is_none() => self.template_name = text,
            Control::TupleValue(id) => {
                let index = self.tuple;
                let mut selected = None;
                self.edit_template(|template| {
                    if let SlotCombinations::ObservedTuples { tuples } = &mut template.combinations
                        && let Some(mut tuple) = tuples.iter().nth(index).cloned()
                    {
                        tuples.remove(&tuple);
                        tuple.insert(id, text);
                        tuples.insert(tuple.clone());
                        selected = tuples.iter().position(|candidate| candidate == &tuple);
                    }
                });
                if let Some(index) = selected {
                    self.tuple = index;
                }
            }
            control => {
                let slot = self.slot;
                self.edit_template(|template| match control {
                    Control::TemplateName => template.name = text,
                    Control::SlotName => {
                        if let Some(slot) = template.slots.get_mut(slot) {
                            slot.label = text;
                        }
                    }
                    Control::DomainValue => {
                        if let Some(slot) = template.slots.get_mut(slot) {
                            match &mut slot.domain {
                                ArgumentDomain::Exact { value } => *value = text,
                                ArgumentDomain::Glob { pattern }
                                | ArgumentDomain::Regex { pattern } => *pattern = text,
                                _ => {}
                            }
                        }
                    }
                    Control::Value(index) => {
                        if let Some(slot) = template.slots.get_mut(slot)
                            && let ArgumentDomain::ObservedSet { values } = &mut slot.domain
                        {
                            if let Some(old) = values.iter().nth(index).cloned() {
                                values.remove(&old);
                            }
                            values.insert(text);
                        }
                    }
                    Control::AddValue => {
                        if let Some(slot) = template.slots.get_mut(slot)
                            && let ArgumentDomain::ObservedSet { values } = &mut slot.domain
                        {
                            values.insert(text);
                        }
                    }
                    _ => {}
                });
            }
        }
        self.editing = None;
        self.field.clear();
        self.changed();
    }

    fn rows(&self) -> Vec<FieldRow> {
        let mut rows = Vec::new();
        let mut add = |control, label: &str, value: String| {
            rows.push(FieldRow {
                control,
                label: label.into(),
                value,
            })
        };
        match self.section {
            Section::Rule => {
                if let Some(reason) = &self.copy_reason {
                    add(
                        Control::Copy,
                        "Copy, not move",
                        format!("{} · {COPY_NOTICE}", safe(reason)),
                    );
                }
                add(
                    Control::Authority,
                    "Registered target",
                    match &self.draft.identity {
                        IdentityDraft::Preserve => {
                            "Preserve existing binding · choose to retarget".into()
                        }
                        IdentityDraft::Registered { key, .. } => safe(key),
                    },
                );
                add(
                    Control::Family,
                    "Capability family",
                    match &self.draft.identity {
                        IdentityDraft::Registered { family, .. } => format!("{family:?}"),
                        _ => "Preserve · select target to change".into(),
                    },
                );
                add(
                    Control::Effect,
                    "Effect",
                    effect_name(&self.draft.effect).into(),
                );
                add(
                    Control::Lifetime,
                    "Lifetime",
                    lifetime_name(&self.draft.lifetime).into(),
                );
                add(
                    Control::Project,
                    "Project binding",
                    format!("{:?}", self.draft.project),
                );
                add(
                    Control::ProjectPath,
                    "Other project",
                    "Explicit path · independent of workdir".into(),
                );
                add(
                    Control::Label,
                    "Display label",
                    self.draft
                        .label
                        .as_deref()
                        .map_or_else(|| "none".into(), literal),
                );
            }
            Section::Targets => {
                add(
                    Control::Target,
                    "Selected target",
                    match &self.draft.resources {
                        ResourcesDraft::Constrained(resources) => {
                            format!("{} / {} · any of", self.target + 1, resources.len())
                        }
                        ResourcesDraft::Unrestricted => "Explicitly UNRESTRICTED".into(),
                        _ => EMPTY_TARGETS.into(),
                    },
                );
                if let Some(resource) = self.resource() {
                    add(
                        Control::SelectorMode,
                        "Match mode",
                        selector_draft_label(&resource.selector),
                    );
                    add(
                        Control::SelectorValue,
                        "Target value",
                        match &resource.selector {
                            SelectorDraft::Replace(value) => selector_edit_text(value).map_or_else(
                                || "Structure / unrestricted".into(),
                                |text| literal(&text),
                            ),
                            _ => format!("{FIXED_VALUE} · choose a mode to replace it"),
                        },
                    );
                    if self.capability().is_some_and(|capability| {
                        capability
                            .selectors
                            .contains(&SelectorMode::CommandTemplate)
                    }) {
                        add(
                            Control::Section(Section::Template),
                            "Command template",
                            if self.template().is_some() {
                                "Edit typed slots; reanalyze structural changes".into()
                            } else {
                                TEMPLATE_CREATE.into()
                            },
                        );
                    }
                    add(
                        Control::Access,
                        "Access",
                        match &resource.access {
                            GuardDraft::Unconfigured => "Not configured",
                            GuardDraft::Any => access_name(None),
                            GuardDraft::Equals(access) => access_name(Some(access)),
                        }
                        .into(),
                    );
                    add(
                        Control::Protection,
                        PROTECTION_LABEL,
                        match resource.protected {
                            GuardDraft::Unconfigured => "Not configured",
                            GuardDraft::Any => "ANY protection",
                            GuardDraft::Equals(true) => "Protected only",
                            GuardDraft::Equals(false) => UNPROTECTED_ONLY,
                        }
                        .into(),
                    );
                    for (name, selector) in &resource.attributes {
                        add(
                            Control::GuardMode(name.clone()),
                            &format!("{name} mode"),
                            selector_draft_label(selector),
                        );
                        add(
                            Control::GuardValue(name.clone()),
                            name,
                            if let SelectorDraft::Replace(value) = selector {
                                selector_edit_text(value).map_or_else(
                                    || "not a text selector".into(),
                                    |text| literal(&text),
                                )
                            } else {
                                format!("{FIXED_VALUE} · kept")
                            },
                        );
                        add(
                            Control::RemoveGuard(name.clone()),
                            "Remove condition",
                            safe(name),
                        );
                    }
                    if let Some(capability) = self.capability() {
                        for name in capability
                            .attributes
                            .keys()
                            .filter(|name| !resource.attributes.contains_key(*name))
                        {
                            add(Control::AddGuard(name.clone()), "Add condition", safe(name));
                        }
                    }
                    add(
                        Control::RemoveTarget,
                        "Remove target",
                        "Last removal leaves an invalid blank, never unrestricted".into(),
                    );
                }
                if let Some(authority) = self.authority() {
                    for (index, capability) in authority.resources.iter().enumerate() {
                        add(
                            Control::AddTarget(index),
                            "Add target",
                            resource_kind(&capability.kind),
                        );
                    }
                }
                add(
                    Control::Unrestricted,
                    "Choose unrestricted",
                    "Explicit authority expansion · requires backend review".into(),
                );
            }
            Section::Arguments => {
                add(
                    Control::ArgumentMode,
                    "Whole-rule input",
                    match self.draft.arguments {
                        ArgumentsDraft::Exact(_) => "Exact JSON input",
                        ArgumentsDraft::Selected { .. } => "Selected JSON pointers",
                        ArgumentsDraft::Unconstrained => "UNCONSTRAINED",
                        ArgumentsDraft::PreserveCoupled => "Explicitly preserve old input pin",
                        ArgumentsDraft::Preserve => "Keep the fixed value",
                        ArgumentsDraft::Unconfigured => "Not configured",
                    }
                    .into(),
                );
                add(
                    Control::Input,
                    "Input value",
                    "Edit a typed JSON value; missing is not null".into(),
                );
                if let ArgumentsDraft::Selected { pointers, .. } = &self.draft.arguments {
                    add(
                        Control::Pointers,
                        "JSON pointers",
                        pointers
                            .iter()
                            .map(|pointer| literal(pointer))
                            .collect::<Vec<_>>()
                            .join(" + "),
                    );
                }
                add(
                    Control::PreserveInput,
                    "Keep old input pin",
                    "Explicitly keep coupled arguments after target changes".into(),
                );
            }
            Section::Template => {
                let template = self.template();
                add(
                    Control::Source,
                    "Source for analysis",
                    if self.source.command.is_empty() {
                        "Explicit concrete command; never executed".into()
                    } else {
                        literal(&self.source.command)
                    },
                );
                add(
                    Control::Workdir,
                    "Analysis workdir",
                    literal(&self.source.workdir.to_string_lossy()),
                );
                if template.is_none() {
                    add(
                        Control::TemplateName,
                        "Template name",
                        literal(&self.template_name),
                    );
                }
                add(
                    Control::Analyze,
                    if template.is_some() {
                        "Analyze structure"
                    } else {
                        "Create template"
                    },
                    if template.is_some() {
                        "Host derives roles, bindings and eligibility".into()
                    } else {
                        TEMPLATE_CREATE.into()
                    },
                );
                if let Some(template) = template {
                    let panel = PatternPanel {
                        definition: Box::new(template.clone()),
                        slot: self.slot,
                        supplied: BTreeSet::new(),
                        show_values: false,
                        caution: None,
                        evidence: String::new(),
                    };
                    for field in panel.fields() {
                        let control = match field.control {
                            PatternControl::Name => Control::TemplateName,
                            PatternControl::Slot => Control::Slot,
                            PatternControl::SlotName => Control::SlotName,
                            PatternControl::Mode => Control::Domain,
                            PatternControl::Constraint => Control::DomainValue,
                            PatternControl::Combinations => Control::Combinations,
                            PatternControl::Observations => Control::SeenEvidence,
                            PatternControl::ObservedValue(_) | PatternControl::SelectSlot(_) => {
                                continue;
                            }
                        };
                        add(control, field.label, field.value);
                    }
                    add(
                        Control::StructureToken,
                        "Structure argument",
                        template.argv.get(self.structure_token).map_or_else(
                            || "No argument selected".into(),
                            |token| {
                                format!(
                                    "argv[{}] {}",
                                    self.structure_token,
                                    match token {
                                        PatternToken::Exact {
                                            role: ArgumentRole::Sensitive | ArgumentRole::Payload,
                                            ..
                                        } => "[redacted]".into(),
                                        PatternToken::Exact { value, role } =>
                                            format!("{role:?} {}", literal(value)),
                                        PatternToken::Slot { id, role } =>
                                            format!("{role:?} ◆{}", id.0),
                                    }
                                )
                            },
                        ),
                    );
                    add(
                        Control::AddSlot,
                        "Add/unlink slot",
                        "Propose an independent slot here · fresh host analysis required".into(),
                    );
                    add(
                        Control::LinkSlot,
                        "Link to selected slot",
                        "Propose repeated equality · host must establish equal source arguments"
                            .into(),
                    );
                    add(
                        Control::StructureLiteral,
                        "Fixed replacement",
                        self.structure_literal
                            .as_deref()
                            .map_or_else(|| "Unset · no value is assumed".into(), literal),
                    );
                    add(
                        Control::RemoveSlot,
                        "Remove selected slot",
                        "Replace every occurrence with the explicit fixed value · reanalyze".into(),
                    );
                    if let Some(slot) = template.slots.get(self.slot)
                        && let ArgumentDomain::ObservedSet { values } = &slot.domain
                    {
                        for (index, value) in values.iter().enumerate() {
                            add(Control::Value(index), "Allowed value", literal(value));
                        }
                        add(
                            Control::AddValue,
                            "Add allowed value",
                            "Manual value · does not add historical support".into(),
                        );
                        add(
                            Control::RemoveValue,
                            "Remove allowed value",
                            format!("Selected value {}", self.value + 1),
                        );
                    }
                    if let SlotCombinations::ObservedTuples { tuples } = &template.combinations {
                        add(
                            Control::Tuple,
                            "Selected tuple",
                            format!("{} / {}", self.tuple + 1, tuples.len()),
                        );
                        if let Some(tuple) = tuples.iter().nth(self.tuple) {
                            for slot in &template.slots {
                                add(
                                    Control::TupleValue(slot.id),
                                    &format!("◆{} {}", slot.id.0, safe(&slot.label)),
                                    tuple
                                        .get(&slot.id)
                                        .map_or_else(|| "MISSING".into(), |value| literal(value)),
                                );
                            }
                        }
                        add(
                            Control::AddTuple,
                            "Add allowed tuple",
                            "Manual combination · not observed evidence".into(),
                        );
                        add(
                            Control::RemoveTuple,
                            "Remove tuple",
                            "Remove selected allowed combination".into(),
                        );
                    }
                }
            }
            Section::Changes => {
                if let Some(preview) = &self.preview {
                    for (control, review) in self.change_reviews() {
                        let (field, before, after) = review.summary();
                        add(control, &field, format!("{before}  →  {after}"));
                    }
                    if !preview.confirmations.is_empty() {
                        add(
                            Control::Confirm,
                            "Confirm authority",
                            if self.confirmed {
                                "[x] Confirmed for this revision"
                            } else {
                                "[ ] I reviewed these authority changes"
                            }
                            .into(),
                        );
                    }
                }
            }
            Section::Test => {
                add(
                    Control::TestInput,
                    "Example JSON input",
                    self.test_input.as_ref().map_or_else(
                        || "Not supplied · distinct from null".into(),
                        |input| safe(&input.to_string()),
                    ),
                );
                add(Control::Section(Section::Test), "Context", "Current session binding; include workdir in tool input only if the host supports it".into());
                add(
                    Control::RunTest,
                    "Test, never execute",
                    self.authority().map_or_else(
                        || NO_AUTHORITY.into(),
                        |authority| format!("Registered target {}", safe(&authority.key)),
                    ),
                );
                match &self.test_state {
                    TestState::Idle => add(
                        Control::Section(Section::Test),
                        "Result",
                        "Not tested; no sample is executed".into(),
                    ),
                    TestState::Pending => add(
                        Control::Section(Section::Test),
                        "Result",
                        "Host evaluation pending; no sample is executed".into(),
                    ),
                    TestState::Complete(Err(error)) => {
                        add(Control::Section(Section::Test), "Unavailable", safe(error))
                    }
                    TestState::Complete(Ok(result)) => {
                        add(
                            Control::Section(Section::Test),
                            "Matches this rule",
                            if result.matches_rule { "YES" } else { "NO" }.into(),
                        );
                        add(
                            Control::Section(Section::Test),
                            "Effective policy",
                            match &result.effective_policy {
                                EffectivePolicyPreview::AllowedByPolicy => {
                                    "Allowed by policy · not an execution promise".into()
                                }
                                EffectivePolicyPreview::Prompt => "PROMPT still required".into(),
                                EffectivePolicyPreview::Denied(reason) => {
                                    format!("DENIED · {}", safe(reason))
                                }
                            },
                        );
                        add(
                            Control::Section(Section::Test),
                            "Execution gates",
                            if result.dispatch_gates_rechecked_at_execution {
                                "Rechecked at execution; this test executes nothing"
                            } else {
                                "Not established by this preview"
                            }
                            .into(),
                        );
                    }
                }
            }
            Section::Scope => {}
        }
        rows
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        if self.suspended || self.is_editing() {
            return;
        }
        if self.section == Section::Scope {
            self.scope_view.scroll(delta.saturating_neg());
        } else if let Control::ChangeDetail(control) = &self.focus
            && let Some(side) = control.side()
        {
            self.changes_view.scroll(side, delta);
        } else {
            self.scroll_rows(delta.saturating_neg());
        }
        self.pressed = None;
    }

    pub(crate) fn scroll_at(&mut self, position: Position, delta: i32) {
        if self.suspended || self.is_editing() || !self.area.contains(position) {
            return;
        }
        if let Some(Hit {
            control: Control::ChangeDetail(control),
            ..
        }) = self.hits.iter().find(|hit| hit.area.contains(position))
            && let Some(side) = control.side()
        {
            self.changes_view.scroll(side, delta);
        } else if self.section == Section::Scope {
            self.scope_view.scroll(delta.saturating_neg());
        } else {
            self.scroll_rows(delta.saturating_neg());
        }
        self.pressed = None;
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> Option<EditorEvent> {
        if self.suspended {
            return None;
        }
        if self.section == Section::Scope && self.scope_view.handle_mouse(event) {
            return None;
        }
        let hit = self
            .hits
            .iter()
            .find(|hit| hit.area.contains(Position::new(event.column, event.row)))
            .cloned();
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.pressed = hit,
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(pressed) = self.pressed.take()
                    && Some(&pressed) == hit.as_ref()
                    && self.editing.is_none()
                {
                    return self.activate(pressed.control);
                }
            }
            MouseEventKind::ScrollDown => {
                self.scroll_at(Position::new(event.column, event.row), -1)
            }
            MouseEventKind::ScrollUp => self.scroll_at(Position::new(event.column, event.row), 1),
            _ => {}
        }
        None
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("permission_scope_editor", area);
        let theme = theme::current();
        self.view_themed(frame, area, &theme);
    }

    fn view_themed(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area != self.area {
            self.pressed = None;
            self.area = area;
            self.reveal_focus = true;
        }
        let old_hits = self.hits.clone();
        self.hits.clear();
        let mut heading = Paragraph::new(format!(
            "EDIT · revision {}{} · [{}] [{}]",
            self.revision,
            if self.is_dirty() { " · unsaved" } else { "" },
            effect_name(&self.draft.effect),
            lifetime_name(&self.draft.lifetime),
        ))
        .style(theme.panel_title)
        .wrap(Wrap { trim: false });
        let heading_rows = heading.line_count(area.width.max(1)) as u16;
        let compact_changes = self.section == Section::Changes
            && area.height < heading_rows + FORM_CHROME_ROWS + DETAIL_ROWS + CHANGE_CHROME_ROWS;
        if compact_changes {
            heading = Paragraph::new(format!(
                "EDIT r{}{}",
                self.revision,
                if self.is_dirty() { " · unsaved" } else { "" }
            ))
            .style(theme.panel_title);
        }
        let help = Paragraph::new(INPUT_HELP)
            .style(theme.item_desc)
            .wrap(Wrap { trim: false });
        let help_height = help.line_count(area.width.max(1)) as u16;
        let [header, tabs, status, body, input, actions] = Layout::vertical([
            Constraint::Length(if compact_changes { 1 } else { heading_rows }),
            Constraint::Length(2),
            Constraint::Length(if compact_changes { 1 } else { 2 }),
            Constraint::Min(1),
            Constraint::Length(u16::from(self.editing.is_some()) * (INPUT_ROWS + help_height)),
            Constraint::Length(if compact_changes { 1 } else { 2 }),
        ])
        .areas(area);
        frame.render_widget(heading, header);
        self.buttons(
            frame,
            tabs,
            [
                (Control::Section(Section::Rule), "[Rule]"),
                (Control::Section(Section::Targets), "[Targets]"),
                (Control::Section(Section::Arguments), "[Arguments]"),
                (Control::Section(Section::Template), "[Template]"),
                (Control::Section(Section::Changes), "[Changes]"),
                (Control::Section(Section::Scope), "[Scope]"),
                (Control::Section(Section::Test), "[Test]"),
            ]
            .into_iter(),
            theme,
        );
        let status_style = if self.preview.is_some() {
            theme.tool_success
        } else {
            theme.tool_warning
        };
        if compact_changes {
            clipped_line(&self.status, status, frame.buffer_mut(), status_style);
        } else {
            frame.render_widget(
                Paragraph::new(self.status.as_str())
                    .style(status_style)
                    .wrap(Wrap { trim: false }),
                status,
            );
        }
        if self.section == Section::Changes {
            self.view_changes(frame, body, theme);
        } else if self.section == Section::Scope {
            let model = self
                .preview
                .as_ref()
                .and_then(|preview| preview.normalized.as_ref())
                .map(|normalized| ScopeModel {
                    source: ScopeSource::Live {
                        rule: Box::new(normalized.rule.clone()),
                        review: normalized.review.clone(),
                        project: normalized.project.clone(),
                    },
                    activity: ScopeActivity::Proposed,
                })
                .or_else(|| self.original.clone());
            if let Some(model) = model {
                self.scope_view
                    .render(&model, body, frame.buffer_mut(), theme);
            } else {
                frame.render_widget(
                    Paragraph::new("Preview a configured draft to inspect its typed scope."),
                    body,
                );
            }
        } else {
            let body = if self.section == Section::Template
                && let Some(template) = self.template()
            {
                let [chips, fields] =
                    Layout::vertical([Constraint::Length(PATTERN_CHIP_ROWS), Constraint::Min(1)])
                        .areas(body);
                let selected = template.slots.get(self.slot).map(|slot| slot.id);
                self.hits.extend(
                    ScopeView::pattern_chips(&template, selected, chips, frame.buffer_mut(), theme)
                        .into_iter()
                        .filter_map(|hit| {
                            if let ScopeControl::Slot(id) = hit.control {
                                Some(Hit {
                                    area: hit.area,
                                    control: Control::SelectSlot(id),
                                })
                            } else {
                                None
                            }
                        }),
                );
                fields
            } else {
                body
            };
            let rows = self.rows();
            self.reveal_row(
                rows.iter().position(|row| row.control == self.focus),
                rows.len(),
                body.height,
            );
            for (index, row) in rows
                .iter()
                .skip(self.offset)
                .take(usize::from(body.height))
                .enumerate()
            {
                let area = Rect {
                    y: body.y + index as u16,
                    height: 1,
                    ..body
                };
                let [label, _, value] = Layout::horizontal([
                    Constraint::Length(FIELD_WIDTH.min(area.width / 2)),
                    Constraint::Length(FIELD_GAP),
                    Constraint::Min(1),
                ])
                .areas(area);
                let style = if row.control == self.focus {
                    theme.item_selected
                } else {
                    theme.item
                };
                clipped_line(&row.label, label, frame.buffer_mut(), style);
                clipped_line(&row.value, value, frame.buffer_mut(), style);
                self.hits.push(Hit {
                    area,
                    control: row.control.clone(),
                });
            }
        }
        if self.editing.is_some() {
            let [value, hint] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(help_height)])
                    .areas(input);
            self.view_input(frame, value, theme);
            frame.render_widget(help, hint);
        }
        self.buttons(
            frame,
            actions,
            [
                (Control::Preview, "[Preview ^P]"),
                (Control::Save, "[Save ^S]"),
                (Control::Cancel, "[Cancel]"),
            ]
            .into_iter(),
            theme,
        );
        self.reveal_focus = false;
        if old_hits != self.hits {
            self.pressed = None;
        }
    }

    fn view_input(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        grab_scope!("permission_scope_editor_input", area);
        let [label, value] = Layout::horizontal([
            Constraint::Length(INPUT_LABEL.width() as u16),
            Constraint::Min(1),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(INPUT_LABEL).style(theme.panel_title), label);
        let styles = FieldStyles {
            caret: theme.cursor,
            ..field_styles(theme.item)
        };
        let cursor = self.field.cursor();
        let width = usize::from(value.width);
        let height = usize::from(value.height);
        let caret_column = render::display_column(&self.field.lines()[cursor.line], cursor.col);
        let pan = (caret_column + 1).saturating_sub(width);
        let lines: Vec<_> = self
            .field
            .lines()
            .iter()
            .enumerate()
            .skip((cursor.line + 1).saturating_sub(height))
            .take(height)
            .map(|(index, line)| {
                let overlays = self.field.overlays(index, &styles, true);
                Row {
                    text: line,
                    segments: None,
                    base: styles.text,
                    fill: None,
                    overlays: &overlays,
                }
                .paint(pan, width)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).style(theme.item), value);
    }

    fn buttons(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        buttons: impl Iterator<Item = (Control, &'static str)>,
        theme: &Theme,
    ) {
        let (mut x, mut y) = (area.x, area.y);
        for (control, label) in buttons {
            let width = (label.width() as u16).min(area.width);
            if x + width > area.right() {
                x = area.x;
                y += 1;
            }
            if y >= area.bottom() {
                break;
            }
            let area = Rect::new(x, y, width, 1);
            let style = if control == self.focus {
                theme.item_selected
            } else {
                theme.keybind_key
            };
            frame.render_widget(Paragraph::new(label).style(style), area);
            self.hits.push(Hit { area, control });
            x += width + 1;
        }
    }

    fn change_reviews(&self) -> Vec<(Control, ChangeReview)> {
        let Some(preview) = &self.preview else {
            return Vec::new();
        };
        preview
            .changes
            .iter()
            .enumerate()
            .map(|(index, change)| {
                (
                    Control::Change(index),
                    ChangeReview::new(
                        change,
                        self.original.as_ref().and_then(ScopeModel::rule),
                        preview.normalized.as_ref(),
                    ),
                )
            })
            .chain(preview.confirmations.iter().map(|requirement| {
                (
                    Control::Requirement(requirement.clone()),
                    ChangeReview::requirement(format!("{requirement:?}")),
                )
            }))
            .collect()
    }

    fn view_changes(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        grab_scope!("permission_scope_editor_changes", area);
        let Some(preview) = &self.preview else {
            frame.render_widget(
                Paragraph::new(NEEDS_PREVIEW).style(theme.tool_warning),
                area,
            );
            return;
        };
        let confirmations = preview.confirmations.len();
        let rows = self.change_reviews();
        let confirmation_rows = u16::from(confirmations > 0);
        let heading_rows = u16::from(area.height >= DETAIL_ROWS + confirmation_rows + 2);
        let detail_rows = if area.height > DETAIL_ROWS + confirmation_rows {
            DETAIL_ROWS
        } else {
            COMPACT_DETAIL_ROWS
        };
        let table_rows = (rows.len().min(CHANGE_ROWS) as u16).min(
            area.height
                .saturating_sub(heading_rows + detail_rows + confirmation_rows),
        );
        let [heading, table, property, confirm] = Layout::vertical([
            Constraint::Length(heading_rows),
            Constraint::Length(table_rows),
            Constraint::Min(1),
            Constraint::Length(confirmation_rows),
        ])
        .areas(area);
        let columns = |row| {
            let mut columns = Layout::horizontal([
                Constraint::Length(FIELD_WIDTH.min(area.width / 3)),
                Constraint::Fill(1),
                Constraint::Fill(1),
            ])
            .areas::<3>(row);
            for column in &mut columns[..2] {
                column.width = column.width.saturating_sub(1);
            }
            columns
        };
        for (column, label) in columns(heading)
            .into_iter()
            .zip(["Field", "Before", "After"])
        {
            frame.render_widget(Paragraph::new(label).style(theme.panel_title), column);
        }
        let selected = rows.iter().position(|(control, _)| *control == self.focus);
        self.reveal_row(selected, rows.len(), table.height);
        let selected = selected
            .filter(|index| {
                *index >= self.offset && *index < self.offset + usize::from(table.height)
            })
            .or(self.changes_view.change.filter(|index| {
                *index >= self.offset && *index < self.offset + usize::from(table.height)
            }))
            .unwrap_or(self.offset);
        self.changes_view.select(selected);
        for (index, (control, review)) in rows
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(usize::from(table.height))
        {
            let row = Rect {
                y: table.y + (index - self.offset) as u16,
                height: 1,
                ..table
            };
            let (field, before, after) = review.summary();
            for ((column, text), style) in columns(row)
                .into_iter()
                .zip([field.as_str(), before.as_str(), after.as_str()])
                .zip([theme.item, theme.diff_old, theme.diff_new])
            {
                clipped_line(
                    text,
                    column,
                    frame.buffer_mut(),
                    if index == selected {
                        theme.item_selected
                    } else {
                        style
                    },
                );
            }
            self.hits.push(Hit {
                area: row,
                control: control.clone(),
            });
        }
        if let Some((_, review)) = rows.get(selected) {
            let focus = if let Control::ChangeDetail(control) = &self.focus {
                Some(control)
            } else {
                None
            };
            self.hits.extend(
                self.changes_view
                    .render(review, property, frame.buffer_mut(), theme, focus)
                    .into_iter()
                    .map(|(area, control)| Hit {
                        area,
                        control: Control::ChangeDetail(control),
                    }),
            );
        }
        if confirmations > 0 && !confirm.is_empty() {
            frame.render_widget(
                Paragraph::new(format!(
                    "[{}] Confirm all {confirmations} changes",
                    if self.confirmed { "x" } else { " " }
                ))
                .style(if self.focus == Control::Confirm {
                    theme.item_selected
                } else {
                    theme.tool_warning
                })
                .wrap(Wrap { trim: false }),
                confirm,
            );
            self.hits.push(Hit {
                area: confirm,
                control: Control::Confirm,
            });
        }
        self.preview_seen =
            property.height >= COMPACT_DETAIL_ROWS && (rows.is_empty() || !table.is_empty());
    }
}

fn selector_edit_text(value: &SelectorValue) -> Option<String> {
    match value {
        SelectorValue::Exact(text)
        | SelectorValue::FilesystemSubtree(text)
        | SelectorValue::UrlSubtree(text)
        | SelectorValue::UrlOrigin(text)
        | SelectorValue::CommandPattern(text) => Some(text.clone()),
        SelectorValue::RemoteExact(parts) | SelectorValue::RemoteSubtree(parts) => {
            Some(parts.join("\n"))
        }
        SelectorValue::Any | SelectorValue::CommandTemplate { .. } => None,
    }
}

fn replace_selector_text(value: &mut SelectorValue, text: String) {
    match value {
        SelectorValue::Exact(value)
        | SelectorValue::FilesystemSubtree(value)
        | SelectorValue::UrlSubtree(value)
        | SelectorValue::UrlOrigin(value)
        | SelectorValue::CommandPattern(value) => *value = text,
        SelectorValue::RemoteExact(parts) | SelectorValue::RemoteSubtree(parts) => {
            *parts = text.lines().map(str::to_owned).collect()
        }
        _ => {}
    }
}

fn selector_draft_label(selector: &SelectorDraft) -> String {
    match selector {
        SelectorDraft::Preserve => "Preserved byte-for-byte".into(),
        SelectorDraft::Unconfigured => "Not configured".into(),
        SelectorDraft::Replace(value) => format!("{:?}", value.mode()),
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::permissions::editor::{
        ArgumentMode, ArgumentsDraft, AuthorityCatalog, AuthorityChange, ConfirmationRequirement,
        EditField, EditableAuthorityDescriptor, EffectivePolicyPreview, IdentityDraft,
        NormalizedPermissionDraft, PermissionMatchPreview, PermissionRuleDraft, ResourceCapability,
        ResourcesDraft, SelectorDraft, SelectorMode, SelectorValue, SemanticChange, VerifiedField,
        VerifiedValue,
    };
    use caudra_agent::permissions::{canonical_json_sha256, selected_input_digest};
    use caudra_agent::tools::registry::TrustedToolSource;
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, PatternDefinition, PatternToken, SlotCombinations, SlotId,
    };
    use caudra_storage::permission_state::{
        PermissionArgumentConstraint, PermissionLifetime, PermissionResourceAccess,
        PermissionResourceKind, PermissionResourceSelector, PermissionSubject,
        StructuredPermissionEffect,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::layout::{Position, Rect};
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;
    use test_case::test_case;

    use super::{
        Control, EditorEvent, EditorLaunch, EditorPreview, INPUT_HELP, MAX_BUFFER_BYTES,
        PREVIEW_DEBOUNCE, PROTECTION_LABEL, STALE_PREVIEW, ScopeEditor, Section,
        TEMPLATE_SOURCE_REQUIRED, TEMPLATE_TARGET_REQUIRED, TestState, UNPROTECTED_ONLY,
        WORKDIR_ATTRIBUTE,
    };
    use crate::components::buffer_text;
    use crate::components::permission_scope::changes::{DetailControl, Side};
    use crate::components::permission_scope::controls::domain_index;
    use crate::components::permission_scope::model::ScopeModel;
    use crate::components::permission_scope::tests::{
        record, template, visual_directory, write_visual_buffer,
    };
    use crate::components::permissions_picker::PermissionsPicker;
    use crate::theme::{self, Theme};

    const LABEL: &str = "Reviewed label";
    const MANUAL: &str = "manually allowed";
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 24;
    const AUTHORITY: &str = "fixture.registered";
    const ANALYSIS_COMMAND: &str = "rg -n error '' error src/main.rs";
    const WORKDIR: &str = "/work/repository";
    const COPY_REASON: &str = "The destination uses a separate database.";
    const FAILURE: &str = "Fresh analysis refused the proposed structure.";
    const SECRET: &str = "never-show-sensitive-payload";
    const QUERY: SlotId = SlotId(1);
    const RULE_MATCH_LABEL: &str = "Matches this rule";
    const POLICY_LABEL: &str = "Effective policy";
    const MATCH_YES: &str = "YES";
    const POLICY_DENIED: &str = "DENIED";
    const SECOND_AUTHORITY: &str = "fixture.second";
    const SORTED_LAST_VALUE: &str = "zz-last";
    const PATH_SLOT: SlotId = SlotId(2);
    const COMMAND_POINTER: &str = "/command";
    const WORKDIR_POINTER: &str = "/workdir";
    const MAX_FOCUS_STEPS: usize = 128;
    const INVALID_JSON: &str = "{";
    const SEED_COMMAND: &str = "rg -n ''";
    const INPUT_TAIL: &str = "cursor-tail";
    const WIDE_CHARACTER: &str = "界";
    const LIMIT_FILL: &str = "a";
    const INPUT_LINE_COUNT: usize = 6;
    const PENDING_REQUIREMENT: &str = "MayReleasePendingRequests";
    const LONG_VALUE_PARTS: usize = 48;
    const BEFORE_PATH_PART: &str = "old-project/source-component/";
    const AFTER_PATH_PART: &str = "new-project/destination-component/";
    const MORE_BELOW: &str = "↓";
    const MORE_ABOVE: &str = "↑";
    const TOKEN_POINTER: &str = "/token";

    fn argument_change_editor(selected: bool) -> ScopeEditor {
        let mut record = record(true);
        let before = record.rule.arguments.clone();
        let input =
            serde_json::json!({"command": ANALYSIS_COMMAND, "workdir": WORKDIR, "token": SECRET});
        record.rule.arguments = if selected {
            let pointers = vec![
                COMMAND_POINTER.to_owned(),
                WORKDIR_POINTER.into(),
                TOKEN_POINTER.into(),
            ];
            PermissionArgumentConstraint::SelectedDigest {
                digest: selected_input_digest(&input, &pointers).unwrap(),
                pointers,
            }
        } else {
            PermissionArgumentConstraint::Exact {
                digest: canonical_json_sha256(&input),
            }
        };
        let changes = vec![SemanticChange::Arguments {
            before,
            after: record.rule.arguments.clone(),
        }];
        let normalized = NormalizedPermissionDraft {
            rule: record.rule,
            project: None,
            label: None,
            review: record.review.unwrap(),
            verified: vec![VerifiedField {
                field: EditField::Arguments,
                value: VerifiedValue::Input(input),
            }],
            opaque: Vec::new(),
        };
        let mut editor = registered_editor();
        editor.receive_preview(
            editor.revision(),
            Ok(EditorPreview {
                normalized: Some(normalized),
                change: AuthorityChange::MixedOrUnknown,
                changes,
                confirmations: [ConfirmationRequirement::UnknownInclusion].into(),
            }),
        );
        editor.activate(Control::Change(0));
        editor
    }

    fn editor() -> ScopeEditor {
        let record = record(true);
        ScopeEditor::new(
            PermissionRuleDraft::from_record(&record, None),
            AuthorityCatalog {
                revision: "fixture".into(),
                authorities: Vec::new(),
            },
            Some(ScopeModel::record(Arc::new(record))),
        )
    }

    fn registered_editor() -> ScopeEditor {
        let mut editor = editor();
        let source = TrustedToolSource::from_mcp_binding(PermissionSubject::Mcp {
            server: "fixture".into(),
            authority: "fixture-authority".into(),
            tool: "fixture-tool".into(),
            contract: "fixture-contract".into(),
        })
        .unwrap();
        editor
            .catalog
            .authorities
            .push(EditableAuthorityDescriptor {
                key: AUTHORITY.into(),
                source,
                resources: vec![ResourceCapability {
                    kind: PermissionResourceKind::Command,
                    selectors: vec![
                        SelectorMode::Exact,
                        SelectorMode::Any,
                        SelectorMode::CommandTemplate,
                    ],
                    access: vec![PermissionResourceAccess::Execute],
                    wildcard_access: true,
                    wildcard_protection: true,
                    attributes: [(WORKDIR_ATTRIBUTE.into(), vec![SelectorMode::Exact])].into(),
                }],
                arguments: vec![
                    ArgumentMode::Exact,
                    ArgumentMode::Selected,
                    ArgumentMode::Unconstrained,
                ],
                families: Vec::new(),
                unrestricted_resources: true,
                unavailable: None,
            });
        editor.draft.identity = IdentityDraft::Registered {
            key: AUTHORITY.into(),
            family: None,
        };
        editor
    }

    fn preview() -> EditorPreview {
        EditorPreview {
            normalized: None,
            change: AuthorityChange::Equivalent,
            changes: Vec::new(),
            confirmations: Default::default(),
        }
    }

    fn literal_template() -> PatternDefinition {
        let mut definition = template();
        definition.name = LABEL.into();
        definition
            .argv
            .retain(|token| matches!(token, PatternToken::Exact { .. }));
        definition.slots.clear();
        definition.combinations = SlotCombinations::Independent;
        definition
    }

    fn review_editor() -> ScopeEditor {
        let mut editor = registered_editor();
        editor.source.command = ANALYSIS_COMMAND.into();
        editor.source.workdir = WORKDIR.into();
        editor.draft.arguments = ArgumentsDraft::Selected {
            input: serde_json::json!({"command": ANALYSIS_COMMAND, "workdir": WORKDIR}),
            pointers: vec![COMMAND_POINTER.into(), WORKDIR_POINTER.into()],
        };
        let preview = EditorPreview {
            normalized: None,
            change: AuthorityChange::MixedOrUnknown,
            changes: vec![
                SemanticChange::Identity,
                SemanticChange::Effect {
                    before: StructuredPermissionEffect::Allow,
                    after: StructuredPermissionEffect::Ask,
                },
                SemanticChange::Lifetime {
                    before: PermissionLifetime::Conversation,
                    after: PermissionLifetime::Project,
                },
                SemanticChange::Project {
                    before: None,
                    after: Some(WORKDIR.into()),
                },
                SemanticChange::Label {
                    before: None,
                    after: Some(LABEL.into()),
                },
                SemanticChange::Resource {
                    index: 0,
                    before: None,
                    after: Some(Box::new(record(true).rule.resources.remove(0))),
                },
            ],
            confirmations: [
                ConfirmationRequirement::ArbitraryExecution,
                ConfirmationRequirement::IndependentCombinations,
                ConfirmationRequirement::MayReleasePendingRequests,
            ]
            .into(),
        };
        editor.receive_preview(editor.revision(), Ok(preview));
        editor
    }

    fn authority_change_editor(long_values: bool) -> ScopeEditor {
        let mut before = record(true).rule.resources.remove(0);
        if long_values {
            let PermissionResourceSelector::CommandTemplate { definition } = &mut before.selector
            else {
                unreachable!()
            };
            definition.slots[1].domain = ArgumentDomain::Glob {
                pattern: BEFORE_PATH_PART.repeat(LONG_VALUE_PARTS),
            };
        }
        let mut after = before.clone();
        let PermissionResourceSelector::CommandTemplate { definition } = &mut after.selector else {
            unreachable!()
        };
        if long_values {
            definition.slots[1].domain = ArgumentDomain::Glob {
                pattern: AFTER_PATH_PART.repeat(LONG_VALUE_PARTS),
            };
        } else {
            definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument;
            definition.combinations = SlotCombinations::Independent;
            after.access = None;
            after.protected = None;
        }
        let mut editor = registered_editor();
        editor.receive_preview(
            editor.revision(),
            Ok(EditorPreview {
                normalized: None,
                change: AuthorityChange::Expansion,
                changes: vec![
                    SemanticChange::Resource {
                        index: 0,
                        before: Some(Box::new(before)),
                        after: Some(Box::new(after)),
                    },
                    SemanticChange::Label {
                        before: None,
                        after: Some(LABEL.into()),
                    },
                ],
                confirmations: [ConfirmationRequirement::ArbitraryExecution].into(),
            }),
        );
        editor.activate(Control::Change(0));
        editor
    }

    fn change_pane(editor: &ScopeEditor, side: &Side) -> Rect {
        editor
            .hits
            .iter()
            .find(|hit| hit.control == Control::ChangeDetail(DetailControl::Pane(side.clone())))
            .unwrap()
            .area
    }

    fn render_size(editor: &mut ScopeEditor, width: u16, height: u16, theme: &Theme) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| editor.view_themed(frame, frame.area(), theme))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_manager(picker: &mut PermissionsPicker, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn click(editor: &mut ScopeEditor, control: Control) -> Option<EditorEvent> {
        let hit = editor
            .hits
            .iter()
            .find(|hit| hit.control == control)
            .unwrap()
            .clone();
        let event = |kind| MouseEvent {
            kind,
            column: hit.area.x,
            row: hit.area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            editor
                .handle_mouse(event(MouseEventKind::Down(MouseButton::Left)))
                .is_none()
        );
        editor.handle_mouse(event(MouseEventKind::Up(MouseButton::Left)))
    }

    fn render(editor: &mut ScopeEditor) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        terminal
    }

    #[test_case(KeyCode::Enter; "enter_applies_field_only")]
    #[test_case(KeyCode::Esc; "escape_cancels_field_only")]
    fn field_text_and_paste_take_precedence_over_actions(code: KeyCode) {
        let mut editor = editor();
        editor.activate(Control::Label);
        editor.handle_paste(LABEL);
        assert!(
            editor
                .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(editor.handle_key(KeyEvent::from(code)).is_none());
        assert_eq!(
            editor.draft.label.as_deref(),
            (code == KeyCode::Enter).then_some(LABEL)
        );
        assert!(editor.preview.is_none());
    }

    #[test_case(false; "suspended")]
    #[test_case(true; "resumed")]
    fn preemption_keeps_dirty_draft_and_rejects_old_preview(resume: bool) {
        let mut editor = editor();
        editor.activate(Control::Label);
        editor.handle_paste(LABEL);
        editor.handle_key(KeyEvent::from(KeyCode::Enter));
        let revision = editor.revision;
        editor.suspend();
        if resume {
            editor.resume(AuthorityCatalog {
                revision: "new".into(),
                authorities: Vec::new(),
            });
        }
        editor.receive_preview(
            revision,
            Ok(EditorPreview {
                normalized: None,
                change: AuthorityChange::Equivalent,
                changes: Vec::new(),
                confirmations: Default::default(),
            }),
        );
        assert_eq!(editor.draft.label.as_deref(), Some(LABEL));
        assert!(editor.is_dirty());
        assert!(editor.preview.is_none());
        assert_eq!(editor.status, STALE_PREVIEW);
    }

    #[test_case(KeyEventKind::Repeat; "held_save")]
    #[test_case(KeyEventKind::Release; "save_release")]
    fn held_or_released_keys_never_save(kind: KeyEventKind) {
        let mut editor = editor();
        editor.receive_preview(
            editor.revision,
            Ok(EditorPreview {
                normalized: None,
                change: AuthorityChange::Equivalent,
                changes: Vec::new(),
                confirmations: Default::default(),
            }),
        );
        render(&mut editor);
        let mut key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        key.kind = kind;
        assert!(editor.handle_key(key).is_none());
    }

    #[test_case(false; "remove_last_target")]
    #[test_case(true; "unrestricted_unavailable")]
    fn empty_targets_require_deliberate_supported_unrestricted_choice(unrestricted: bool) {
        let mut editor = editor();
        editor.activate(Control::RemoveTarget);
        if unrestricted {
            editor.activate(Control::Unrestricted);
        }
        assert!(matches!(
            editor.draft.resources,
            ResourcesDraft::Unconfigured
        ));
    }

    #[test_case(false; "manual_value")]
    #[test_case(true; "manual_empty_value")]
    fn manual_values_do_not_modify_historical_source(empty: bool) {
        let mut editor = editor();
        let original = editor.original.clone();
        editor.activate(Control::AddValue);
        if !empty {
            editor.handle_paste(MANUAL);
        }
        editor.handle_key(KeyEvent::from(KeyCode::Enter));
        let template = editor.template().unwrap();
        let ArgumentDomain::ObservedSet { values } = &template.slots[0].domain else {
            panic!("expected allowed set");
        };
        assert!(values.contains(if empty { "" } else { MANUAL }));
        assert_eq!(editor.original, original);
    }

    #[test_case(false; "geometry_change")]
    #[test_case(true; "form_change")]
    fn stale_mouse_release_cannot_activate_save(form_change: bool) {
        let mut editor = editor();
        render(&mut editor);
        let hit = editor
            .hits
            .iter()
            .find(|hit| hit.control == Control::Save)
            .unwrap()
            .clone();
        let event = |kind| MouseEvent {
            kind,
            column: hit.area.x,
            row: hit.area.y,
            modifiers: KeyModifiers::NONE,
        };
        editor.handle_mouse(event(MouseEventKind::Down(MouseButton::Left)));
        if form_change {
            editor.activate(Control::Section(Section::Arguments));
        }
        let mut terminal = Terminal::new(TestBackend::new(
            if form_change { WIDTH } else { WIDTH / 2 },
            HEIGHT,
        ))
        .unwrap();
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        assert!(!matches!(
            editor.handle_mouse(event(MouseEventKind::Up(MouseButton::Left))),
            Some(EditorEvent::Save { .. })
        ));
    }

    #[test_case(Section::Rule; "rule")]
    #[test_case(Section::Targets; "targets")]
    #[test_case(Section::Template; "template")]
    fn changing_forms_keeps_actions_at_stable_cells_and_styles(section: Section) {
        let mut editor = editor();
        let before = render(&mut editor);
        let actions: Vec<_> = editor
            .hits
            .iter()
            .filter(|hit| {
                matches!(
                    hit.control,
                    Control::Preview | Control::Save | Control::Cancel
                )
            })
            .cloned()
            .collect();
        editor.activate(Control::Section(section));
        let after = render(&mut editor);
        for action in actions {
            assert!(editor.hits.contains(&action));
            for x in action.area.x..action.area.right() {
                assert_eq!(
                    before.backend().buffer()[(x, action.area.y)],
                    after.backend().buffer()[(x, action.area.y)]
                );
            }
        }
    }

    #[test_case('s'; "save")]
    #[test_case('p'; "preview")]
    #[test_case('n'; "new")]
    #[test_case('b'; "copy")]
    #[test_case('c'; "close")]
    fn root_shortcuts_are_captured_while_enter_applies_only_the_field(shortcut: char) {
        let mut editor = editor();
        editor.receive_preview(editor.revision(), Ok(preview()));
        render(&mut editor);
        editor.activate(Control::Label);
        editor.handle_paste(LABEL);
        assert!(
            editor
                .handle_key(KeyEvent::new(
                    KeyCode::Char(shortcut),
                    KeyModifiers::CONTROL
                ))
                .is_none()
        );
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(editor.preview.is_none());
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(editor.editing.is_none());
    }

    #[test_case(false; "label_change")]
    #[test_case(true; "effect_change")]
    fn debounce_emits_only_latest_revision_without_moving_focus(effect: bool) {
        let mut editor = editor();
        assert!(editor.poll_preview(Duration::ZERO).is_none());
        if effect {
            editor.activate(Control::Effect);
        } else {
            editor.activate(Control::Label);
            editor.handle_paste(LABEL);
            editor.handle_key(KeyEvent::from(KeyCode::Enter));
        }
        let time = PREVIEW_DEBOUNCE / 2;
        assert!(editor.poll_preview(time).is_none());
        assert!(editor.poll_preview(PREVIEW_DEBOUNCE).is_none());
        let focus = editor.focus.clone();
        let Some(EditorEvent::Preview { revision, draft }) =
            editor.poll_preview(time + PREVIEW_DEBOUNCE)
        else {
            panic!("expected debounced preview");
        };
        assert_eq!(draft.as_ref(), editor.draft());
        assert_eq!(revision, editor.revision());
        editor.receive_preview(revision, Ok(preview()));
        assert_eq!(editor.focus, focus);
        assert_eq!(editor.section, Section::Rule);
        assert!(editor.poll_preview(time + PREVIEW_DEBOUNCE * 2).is_none());
    }

    #[test_case(false; "uncommitted_buffer")]
    #[test_case(true; "committed_draft")]
    fn visibility_hook_keeps_edits_and_restarts_debounce(committed: bool) {
        let mut editor = editor();
        editor.activate(Control::Label);
        editor.handle_paste(LABEL);
        if committed {
            editor.handle_key(KeyEvent::from(KeyCode::Enter));
        }
        let revision = editor.revision();
        editor.set_visible(false);
        let suspended_revision = editor.revision();
        editor.set_visible(false);
        assert!(editor.is_suspended());
        assert_eq!(editor.revision(), suspended_revision);
        assert!(editor.poll_preview(PREVIEW_DEBOUNCE * 10).is_none());
        let text = editor.field.text();
        editor.handle_paste(MANUAL);
        assert_eq!(editor.field.text(), text);
        editor.set_visible(true);
        editor.receive_preview(revision, Ok(preview()));
        assert!(editor.preview.is_none());
        assert!(!editor.is_suspended());
        assert!(editor.is_dirty());
        if committed {
            assert_eq!(editor.draft().label.as_deref(), Some(LABEL));
        } else {
            assert_eq!(editor.field.text(), LABEL);
            assert!(editor.editing.is_some());
        }
    }

    #[test_case(false; "no_implicit_copy")]
    #[test_case(true; "explicit_copy")]
    fn failed_move_offers_copy_without_chaining_revoke(copy: bool) {
        let mut editor = editor();
        editor.activate(Control::Lifetime);
        let before = editor.original.clone();
        let draft = editor.draft().clone();
        editor.offer_copy(COPY_REASON);
        assert!(editor.preview.is_none());
        assert!(editor.rows().iter().any(|row| row.control == Control::Copy));
        if copy {
            let Some(EditorEvent::Begin(EditorLaunch::Copy {
                source,
                draft: Some(proposed),
            })) = editor.activate(Control::Copy)
            else {
                panic!("expected independent Copy launch");
            };
            assert_eq!(*proposed, draft);
            let Some(ScopeModel {
                source: super::ScopeSource::Record(expected),
                ..
            }) = before.as_ref()
            else {
                panic!("expected stored source");
            };
            assert_eq!(&source, expected);
        }
        assert_eq!(editor.original, before);
        assert_eq!(editor.draft(), &draft);
    }

    #[test_case(Control::AddSlot; "add_slot")]
    #[test_case(Control::RemoveSlot; "remove_slot")]
    #[test_case(Control::LinkSlot; "link_slot")]
    fn structure_changes_are_only_applied_by_current_host_analysis(control: Control) {
        let mut editor = registered_editor();
        editor.source.command = ANALYSIS_COMMAND.into();
        editor.source.workdir = WORKDIR.into();
        editor.structure_token = 3;
        editor.structure_literal = Some(MANUAL.into());
        let before = editor.draft().clone();
        let Some(EditorEvent::Analyze {
            revision,
            target,
            source,
            proposed,
            authority_key,
        }) = editor.activate(control)
        else {
            panic!("expected fresh host analysis");
        };
        assert_eq!(authority_key, AUTHORITY);
        assert_eq!(editor.draft(), &before);
        assert!(editor.poll_preview(PREVIEW_DEBOUNCE * 10).is_none());
        editor.set_analyzed_template(
            revision.saturating_sub(1),
            target,
            *proposed.clone(),
            source.clone(),
        );
        assert_eq!(editor.draft(), &before);
        editor.set_analyzed_template(revision, target, *proposed.clone(), source);
        assert_eq!(editor.template().as_ref(), Some(proposed.as_ref()));
        assert_ne!(editor.draft(), &before);
    }

    #[test_case(false; "analysis_failure")]
    #[test_case(true; "analysis_after_suspend")]
    fn failed_or_preempted_analysis_keeps_original_structure(suspend: bool) {
        let mut editor = registered_editor();
        editor.source.command = ANALYSIS_COMMAND.into();
        editor.source.workdir = WORKDIR.into();
        editor.structure_token = 3;
        let before = editor.draft().clone();
        let Some(EditorEvent::Analyze {
            revision,
            target,
            source,
            proposed,
            ..
        }) = editor.activate(Control::AddSlot)
        else {
            panic!("expected analysis request");
        };
        if suspend {
            editor.set_visible(false);
        } else {
            editor.analysis_failed(revision, FAILURE);
        }
        editor.set_analyzed_template(revision, target, *proposed, source);
        assert_eq!(editor.draft(), &before);
    }

    #[test_case(0; "allowed_values")]
    #[test_case(1; "exact")]
    #[test_case(2; "glob")]
    #[test_case(3; "regex")]
    #[test_case(4; "one_literal")]
    fn every_slot_domain_preserves_tuples_roles_and_source_evidence(mode: usize) {
        let mut editor = editor();
        let original = editor.original.clone();
        let before = editor.template().unwrap();
        for _ in 0..mode {
            editor.activate(Control::Domain);
        }
        let after = editor.template().unwrap();
        assert_eq!(domain_index(&after.slots[0].domain), mode);
        assert_eq!(after.argv, before.argv);
        assert_eq!(after.combinations, before.combinations);
        assert_eq!(editor.original, original);
    }

    #[test_case(false; "resource_any")]
    #[test_case(true; "unrestricted_resources")]
    fn empty_and_any_targets_remain_distinct_explicit_choices(unrestricted: bool) {
        let mut editor = registered_editor();
        editor.activate(Control::RemoveTarget);
        assert!(matches!(
            editor.draft().resources,
            ResourcesDraft::Unconfigured
        ));
        if unrestricted {
            editor.activate(Control::Unrestricted);
            assert!(matches!(
                editor.draft().resources,
                ResourcesDraft::Unrestricted
            ));
        } else {
            editor.activate(Control::AddTarget(0));
            editor.activate(Control::SelectorMode);
            editor.activate(Control::SelectorMode);
            let ResourcesDraft::Constrained(resources) = &editor.draft().resources else {
                panic!("expected explicit target alternative");
            };
            assert_eq!(resources.len(), 1);
            assert_eq!(
                resources[0].selector,
                SelectorDraft::Replace(SelectorValue::Any)
            );
        }
    }

    #[test_case(false; "same_rule")]
    #[test_case(true; "new_revision")]
    fn test_examples_never_authorize_and_stale_results_are_rejected(stale: bool) {
        let mut editor = registered_editor();
        editor.receive_preview(editor.revision(), Ok(preview()));
        editor.test_input = Some(serde_json::json!({"path": null}));
        let before = editor.draft().clone();
        let Some(EditorEvent::Test {
            revision,
            test_revision,
            example,
        }) = editor.activate(Control::RunTest)
        else {
            panic!("expected host example evaluation");
        };
        assert_eq!(example.authority_key, AUTHORITY);
        assert_eq!(example.input, serde_json::json!({"path": null}));
        if stale {
            editor.activate(Control::Effect);
        }
        editor.receive_test(
            revision,
            test_revision,
            Ok(PermissionMatchPreview {
                matches_rule: true,
                effective_policy: EffectivePolicyPreview::Denied(FAILURE.into()),
                dispatch_gates_rechecked_at_execution: true,
            }),
        );
        editor.section = Section::Test;
        let rows = editor.rows();
        if stale {
            assert!(matches!(editor.test_state, TestState::Idle));
        } else {
            assert!(
                rows.iter()
                    .any(|row| row.label == RULE_MATCH_LABEL && row.value == MATCH_YES)
            );
            assert!(
                rows.iter()
                    .any(|row| row.label == POLICY_LABEL && row.value.contains(POLICY_DENIED))
            );
            assert_eq!(editor.draft(), &before);
        }
    }

    #[test_case(ArgumentRole::Sensitive; "sensitive")]
    #[test_case(ArgumentRole::Payload; "payload")]
    fn selected_structure_argument_never_discloses_sensitive_literals(role: ArgumentRole) {
        let mut editor = editor();
        editor.edit_template(|template| {
            template.argv.push(PatternToken::Exact {
                value: SECRET.into(),
                role,
            })
        });
        editor.structure_token = editor.template().unwrap().argv.len() - 1;
        editor.activate(Control::Section(Section::Template));
        let rows = editor.rows();
        let selected = rows
            .iter()
            .find(|row| row.control == Control::StructureToken)
            .unwrap();
        assert!(selected.value.contains("[redacted]"));
        assert!(rows.iter().all(|row| !row.value.contains(SECRET)));
        assert!(!buffer_text(render(&mut editor).backend().buffer()).contains(SECRET));
    }

    #[test_case(KeyCode::Tab, Section::Test; "next_tab")]
    #[test_case(KeyCode::BackTab, Section::Changes; "previous_tab")]
    fn scope_navigation_does_not_capture_other_sections(key: KeyCode, expected: Section) {
        let mut editor = editor();
        editor.activate(Control::SeenEvidence);
        render(&mut editor);
        editor.handle_key(KeyEvent::from(key));
        assert_eq!(editor.focus, Control::Section(expected.clone()));
        editor.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(editor.section, expected);
        assert!(editor.scope_view.slot.is_none());
    }

    #[test_case(false; "allowed_tuple")]
    #[test_case(true; "independent_combinations")]
    fn manual_combinations_never_become_historical_evidence(independent: bool) {
        let mut editor = editor();
        let original = editor.original.clone();
        let before = editor.template().unwrap();
        if independent {
            editor.activate(Control::Combinations);
            assert_eq!(
                editor.template().unwrap().combinations,
                SlotCombinations::Independent
            );
        } else {
            editor.activate(Control::AddTuple);
            editor.activate(Control::TupleValue(QUERY));
            editor.handle_paste(MANUAL);
            editor.handle_key(KeyEvent::from(KeyCode::Enter));
            let SlotCombinations::ObservedTuples { tuples } =
                editor.template().unwrap().combinations
            else {
                panic!("expected allowed tuples");
            };
            assert!(
                tuples
                    .iter()
                    .any(|tuple| tuple.get(&QUERY).is_some_and(|value| value == MANUAL))
            );
            let SlotCombinations::ObservedTuples { tuples: previous } = &before.combinations else {
                panic!("expected original listed tuples");
            };
            assert!(previous.is_subset(&tuples));
        }
        assert_eq!(editor.template().unwrap().argv, before.argv);
        assert_eq!(editor.original, original);
    }

    #[test_case(false; "wrong_target")]
    #[test_case(true; "wrong_source")]
    fn analysis_reply_must_match_requested_source_and_target(wrong_source: bool) {
        let mut editor = registered_editor();
        editor.source.command = ANALYSIS_COMMAND.into();
        editor.source.workdir = WORKDIR.into();
        let before = editor.draft().clone();
        let Some(EditorEvent::Analyze {
            revision,
            mut target,
            mut source,
            proposed,
            ..
        }) = editor.activate(Control::Analyze)
        else {
            panic!("expected analysis request");
        };
        if wrong_source {
            source.command = MANUAL.into();
        } else {
            target += 1;
        }
        editor.set_analyzed_template(revision, target, *proposed, source);
        assert_eq!(editor.draft(), &before);
        assert!(editor.analysis_pending.is_some());
    }

    #[test_case(KeyCode::Enter; "apply")]
    #[test_case(KeyCode::Esc; "cancel")]
    fn editing_api_reports_uncommitted_fields_across_suspension(exit: KeyCode) {
        let mut editor = editor();
        assert!(!editor.is_editing());
        editor.activate(Control::Label);
        editor.handle_paste(LABEL);
        assert!(editor.is_editing());
        editor.suspend();
        assert!(editor.is_editing());
        editor.set_visible(true);
        assert!(editor.is_editing());
        editor.handle_key(KeyEvent::from(exit));
        assert!(!editor.is_editing());
        assert_eq!(
            editor.draft.label.as_deref(),
            (exit == KeyCode::Enter).then_some(LABEL)
        );
    }

    #[test_case(Control::Input, Section::Arguments; "rule_input")]
    #[test_case(Control::TestInput, Section::Test; "example_input")]
    fn invalid_json_keeps_editing_and_blocks_save(control: Control, section: Section) {
        let mut editor = review_editor();
        let before = editor.draft().clone();
        editor.activate(Control::Section(section));
        editor.activate(control);
        editor.field.clear();
        editor.handle_paste(INVALID_JSON);
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(editor.is_editing());
        assert!(editor.activate(Control::Save).is_none());
        assert_eq!(editor.draft(), &before);
    }

    #[test_case(false; "unconfigured")]
    #[test_case(true; "exact_selector")]
    fn first_template_waits_for_host_and_pins_returned_workdir(exact: bool) {
        let mut editor = registered_editor();
        editor.resource_mut().unwrap().selector = if exact {
            SelectorDraft::Replace(SelectorValue::Exact(SEED_COMMAND.into()))
        } else {
            SelectorDraft::Unconfigured
        };
        editor.activate(Control::Section(Section::Targets));
        assert!(
            editor
                .rows()
                .iter()
                .any(|row| row.control == Control::Section(Section::Template))
        );
        editor.activate(Control::Section(Section::Template));
        editor.activate(Control::TemplateName);
        editor.field.clear();
        editor.handle_paste(LABEL);
        editor.handle_key(KeyEvent::from(KeyCode::Enter));
        editor.source.command = SEED_COMMAND.into();
        editor.source.workdir = WORKDIR.into();
        let before = editor.draft().clone();
        let Some(EditorEvent::Seed {
            revision,
            target,
            authority_key,
            source,
            name,
        }) = editor.activate(Control::Analyze)
        else {
            panic!("expected seed request");
        };
        assert_eq!(authority_key, AUTHORITY);
        assert_eq!(name, LABEL);
        assert_eq!(source, editor.source);
        assert_eq!(editor.draft(), &before);
        assert!(editor.template().is_none());
        assert!(editor.poll_preview(PREVIEW_DEBOUNCE).is_none());
        assert!(editor.activate(Control::Save).is_none());
        let definition = literal_template();
        editor.set_analyzed_template(revision, target, definition.clone(), source.clone());
        assert_eq!(editor.template(), Some(definition.clone()));
        assert_eq!(
            editor.resource().unwrap().attributes.get(WORKDIR_ATTRIBUTE),
            Some(&SelectorDraft::Replace(SelectorValue::Exact(
                definition.context.effective_workdir
            )))
        );
        let SelectorDraft::Replace(SelectorValue::CommandTemplate {
            source: retained, ..
        }) = &editor.resource().unwrap().selector
        else {
            panic!("expected seeded template");
        };
        assert_eq!(retained.as_ref(), Some(&source));
        assert!(editor.revision() > revision);
        assert!(editor.analysis_pending.is_none());
        assert!(editor.preview.is_none());
        assert!(!editor.confirmed);
        assert!(editor.activate(Control::Save).is_none());
        assert!(matches!(
            editor.activate(Control::Preview),
            Some(EditorEvent::Preview { .. })
        ));
    }

    #[test_case(false; "missing_command")]
    #[test_case(true; "relative_workdir")]
    fn seed_requires_explicit_source_and_absolute_workdir(relative: bool) {
        let mut editor = registered_editor();
        editor.resource_mut().unwrap().selector = SelectorDraft::Unconfigured;
        editor.source.command = if relative {
            SEED_COMMAND.into()
        } else {
            String::new()
        };
        editor.source.workdir = if relative { "relative" } else { WORKDIR }.into();
        let before = editor.draft().clone();
        assert!(editor.activate(Control::Analyze).is_none());
        assert_eq!(editor.status, TEMPLATE_SOURCE_REQUIRED);
        assert_eq!(editor.draft(), &before);
    }

    #[test_case(false; "no_target")]
    #[test_case(true; "no_template_capability")]
    fn seed_requires_a_supported_selected_target(unsupported: bool) {
        let mut editor = registered_editor();
        if unsupported {
            editor.catalog.authorities[0].resources[0].selectors.clear();
        } else {
            editor.draft.resources = ResourcesDraft::Unconfigured;
        }
        editor.source.command = SEED_COMMAND.into();
        editor.source.workdir = WORKDIR.into();
        let before = editor.draft().clone();
        assert!(editor.activate(Control::Analyze).is_none());
        assert_eq!(editor.status, TEMPLATE_TARGET_REQUIRED);
        assert_eq!(editor.draft(), &before);
    }

    #[test_case(false; "revision_changed")]
    #[test_case(true; "suspended")]
    fn stale_seed_reply_never_configures_target(suspend: bool) {
        let mut editor = registered_editor();
        editor.resource_mut().unwrap().selector = SelectorDraft::Unconfigured;
        editor.source.command = SEED_COMMAND.into();
        editor.source.workdir = WORKDIR.into();
        let Some(EditorEvent::Seed {
            revision,
            target,
            source,
            ..
        }) = editor.activate(Control::Analyze)
        else {
            panic!("expected seed request");
        };
        if suspend {
            editor.suspend();
        } else {
            editor.activate(Control::Effect);
        }
        let before = editor.draft().clone();
        editor.set_analyzed_template(revision, target, literal_template(), source);
        assert_eq!(editor.draft(), &before);
        assert!(editor.template().is_none());
    }

    #[test_case(false; "skip_disabled")]
    #[test_case(true; "none_available")]
    fn unavailable_authorities_do_not_trap_selection(all_disabled: bool) {
        let mut editor = registered_editor();
        let mut unavailable = editor.catalog.authorities[0].clone();
        unavailable.key = FAILURE.into();
        unavailable.unavailable = Some(FAILURE.into());
        let mut available = editor.catalog.authorities[0].clone();
        available.key = SECOND_AUTHORITY.into();
        editor.catalog.authorities.extend([unavailable, available]);
        if all_disabled {
            for authority in &mut editor.catalog.authorities {
                authority.unavailable = Some(FAILURE.into());
            }
        }
        editor.activate(Control::Authority);
        assert_eq!(
            editor.draft.identity,
            IdentityDraft::Registered {
                key: if all_disabled {
                    AUTHORITY
                } else {
                    SECOND_AUTHORITY
                }
                .into(),
                family: None
            }
        );
    }

    #[test_case(SelectorMode::Exact; "exact")]
    #[test_case(SelectorMode::Any; "any")]
    fn template_analysis_does_not_block_other_selector_modes(start: SelectorMode) {
        let mut editor = registered_editor();
        editor.resource_mut().unwrap().selector =
            SelectorDraft::Replace(if start == SelectorMode::Any {
                SelectorValue::Any
            } else {
                SelectorValue::Exact(String::new())
            });
        editor.activate(Control::SelectorMode);
        let SelectorDraft::Replace(value) = &editor.resource().unwrap().selector else {
            panic!("expected chosen selector");
        };
        assert_ne!(value.mode(), start);
        assert_ne!(value.mode(), SelectorMode::CommandTemplate);
    }

    #[test_case(KeyModifiers::SHIFT; "shift_enter")]
    #[test_case(KeyModifiers::CONTROL; "control_enter")]
    fn multiline_fields_accept_newlines_without_committing(modifiers: KeyModifiers) {
        let mut editor = review_editor();
        editor.activate(Control::Pointers);
        editor.field.clear();
        editor.handle_paste(COMMAND_POINTER);
        assert!(
            editor
                .handle_key(KeyEvent::new(KeyCode::Enter, modifiers))
                .is_none()
        );
        assert!(editor.is_editing());
        editor.handle_paste(WORKDIR_POINTER);
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        let ArgumentsDraft::Selected { pointers, .. } = &editor.draft.arguments else {
            panic!("expected selected pointers");
        };
        assert_eq!(pointers, &[COMMAND_POINTER, WORKDIR_POINTER]);
    }

    #[test_case(false; "keyboard")]
    #[test_case(true; "mouse")]
    fn field_edits_cannot_trigger_save_even_with_a_confirmed_preview(mouse: bool) {
        let mut editor = review_editor();
        render(&mut editor);
        editor.activate(Control::Confirm);
        editor.activate(Control::Section(Section::Test));
        editor.activate(Control::TestInput);
        editor.handle_paste("null");
        render(&mut editor);
        if mouse {
            assert!(click(&mut editor, Control::Save).is_none());
        } else {
            assert!(
                editor
                    .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                    .is_none()
            );
        }
        assert!(editor.is_editing());
        assert!(editor.activate(Control::Save).is_none());
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(!editor.is_editing());
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(matches!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(EditorEvent::Save { .. })
        ));
    }

    #[test_case(false; "existing_tuple")]
    #[test_case(true; "new_tuple")]
    fn tuple_selection_follows_the_edited_tuple_after_sorting(new_tuple: bool) {
        let mut editor = editor();
        if new_tuple {
            editor.activate(Control::Tuple);
            editor.activate(Control::AddTuple);
        }
        editor.activate(Control::TupleValue(QUERY));
        editor.field.clear();
        editor.handle_paste(SORTED_LAST_VALUE);
        editor.handle_key(KeyEvent::from(KeyCode::Enter));
        editor.activate(Control::TupleValue(PATH_SLOT));
        editor.field.clear();
        editor.handle_paste(MANUAL);
        editor.handle_key(KeyEvent::from(KeyCode::Enter));
        let SlotCombinations::ObservedTuples { tuples } = editor.template().unwrap().combinations
        else {
            panic!("expected tuples");
        };
        let tuple = tuples.iter().nth(editor.tuple).unwrap();
        assert_eq!(tuple[&QUERY], SORTED_LAST_VALUE);
        assert_eq!(tuple[&PATH_SLOT], MANUAL);
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn long_multiline_input_keeps_the_caret_and_edit_help_visible(width: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let theme = theme::load_by_name(name).unwrap();
            let mut editor = review_editor();
            editor.activate(Control::Section(Section::Test));
            editor.activate(Control::TestInput);
            editor.field.clear();
            let input = format!(
                "{}{}{INPUT_TAIL}",
                format!("{WORKDIR_POINTER}\n").repeat(INPUT_LINE_COUNT),
                WIDE_CHARACTER.repeat(usize::from(width)),
            );
            editor.handle_paste(&input);
            let caret =
                |cell: &Cell| Some(cell.fg) == theme.cursor.fg && Some(cell.bg) == theme.cursor.bg;
            let buffer = render_size(&mut editor, width, HEIGHT, &theme);
            let text = buffer_text(&buffer);
            assert!(text.contains(INPUT_TAIL));
            assert!(INPUT_HELP.lines().all(|line| text.contains(line)));
            let carets: Vec<_> = buffer.content.iter().filter(|cell| caret(cell)).collect();
            assert_eq!(carets.len(), 1, "{buffer:#?}");
            assert_eq!(carets[0].symbol(), " ");
            editor.handle_key(KeyEvent::from(KeyCode::Home));
            let buffer = render_size(&mut editor, width, HEIGHT, &theme);
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| cell.symbol() == WIDE_CHARACTER && caret(cell))
            );
            editor.handle_key(KeyEvent::from(KeyCode::Up));
            let buffer = render_size(&mut editor, width, HEIGHT, &theme);
            assert!(buffer_text(&buffer).contains(WORKDIR_POINTER));
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| cell.symbol() == "/" && caret(cell))
            );
            assert_eq!(editor.field.text(), input);
            assert!(editor.is_editing());
        }
    }

    #[test_case(KeyCode::Backspace, KeyModifiers::NONE, true; "backspace_deletes")]
    #[test_case(KeyCode::Char('w'), KeyModifiers::CONTROL, true; "word_delete_deletes")]
    #[test_case(KeyCode::Char('x'), KeyModifiers::NONE, false; "typing_is_refused")]
    #[test_case(KeyCode::Enter, KeyModifiers::SHIFT, false; "newline_is_refused")]
    fn input_at_the_byte_limit_only_shrinks(code: KeyCode, modifiers: KeyModifiers, shrinks: bool) {
        let mut editor = editor();
        editor.activate(Control::Label);
        editor.handle_paste(&LIMIT_FILL.repeat(MAX_BUFFER_BYTES));
        editor.handle_paste(LIMIT_FILL);
        assert_eq!(editor.field.byte_len(), MAX_BUFFER_BYTES);
        assert!(editor.handle_key(KeyEvent::new(code, modifiers)).is_none());
        assert_eq!(editor.field.byte_len() < MAX_BUFFER_BYTES, shrinks);
        assert!(editor.is_editing());
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn selected_requirement_is_readable_in_the_manager(width: u16) {
        let mut editor = review_editor();
        editor.activate(Control::Section(Section::Changes));
        editor.activate(Control::Requirement(
            ConfirmationRequirement::MayReleasePendingRequests,
        ));
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record(true)], &[], &[], false, false);
        picker.set_editor(editor);
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let editor = picker.editor_mut().unwrap();
        let table_bottom = editor
            .hits
            .iter()
            .filter(|hit| matches!(hit.control, Control::Change(_) | Control::Requirement(_)))
            .map(|hit| hit.area.bottom())
            .max()
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!((table_bottom..buffer.area.bottom()).any(|y| {
            let line: String = (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            line.contains(PENDING_REQUIREMENT)
        }));
        assert!(!editor.confirmed);
    }

    #[test_case(40, 16; "narrow_short")]
    #[test_case(80, 24; "normal")]
    #[test_case(140, 32; "wide")]
    fn every_form_control_is_revealed_by_keyboard_focus(width: u16, height: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let theme = theme::load_by_name(name).unwrap();
            for section in [
                Section::Rule,
                Section::Targets,
                Section::Arguments,
                Section::Template,
                Section::Changes,
                Section::Test,
            ] {
                let mut editor = review_editor();
                editor.activate(Control::Section(section));
                let expected: Vec<_> = editor.rows().into_iter().map(|row| row.control).collect();
                let mut visited = Vec::new();
                for _ in 0..MAX_FOCUS_STEPS {
                    assert!(editor.handle_key(KeyEvent::from(KeyCode::Tab)).is_none());
                    let buffer = render_size(&mut editor, width, height, &theme);
                    let hit = editor
                        .hits
                        .iter()
                        .find(|hit| hit.control == editor.focus)
                        .unwrap_or_else(|| panic!("unrevealed {:?}", editor.focus));
                    assert!(!hit.area.is_empty());
                    assert_eq!(hit.area.intersection(buffer.area), hit.area);
                    visited.push(editor.focus.clone());
                    if expected.iter().all(|control| visited.contains(control)) {
                        break;
                    }
                }
                assert!(expected.iter().all(|control| visited.contains(control)));
                let focused = editor.focus.clone();
                editor.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
                editor.handle_key(KeyEvent::from(KeyCode::Tab));
                assert_eq!(editor.focus, focused);
            }
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn change_details_reach_every_old_and_new_cell_without_moving_the_list(width: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let theme = theme::load_by_name(name).unwrap();
            for side in [Side::Before, Side::After] {
                let mut editor = authority_change_editor(true);
                render_size(&mut editor, width, HEIGHT, &theme);
                assert!(
                    editor
                        .activate(Control::ChangeDetail(DetailControl::Pane(side.clone())))
                        .is_none()
                );
                let initial = render_size(&mut editor, width, HEIGHT, &theme);
                let pane = change_pane(&editor, &side);
                let body = Rect::new(pane.x, pane.y + 1, pane.width, pane.height - 2);
                let other = change_pane(
                    &editor,
                    &if side == Side::Before {
                        Side::After
                    } else {
                        Side::Before
                    },
                );
                let anchors: Vec<_> = editor
                    .hits
                    .iter()
                    .filter(|hit| {
                        matches!(hit.control, Control::Change(_) | Control::Requirement(_))
                    })
                    .cloned()
                    .collect();
                let (_, review) = editor.change_reviews().remove(0);
                let predicate = &review.predicates[0];
                let (value, style) = if side == Side::Before {
                    (&predicate.before, theme.diff_old)
                } else {
                    (&predicate.after, theme.diff_new)
                };
                let paragraph = Paragraph::new(vec![
                    Line::from(value.clone()),
                    Line::styled(predicate.label.clone(), theme.item_desc),
                ])
                .style(style)
                .wrap(Wrap { trim: false });
                let total = paragraph.line_count(body.width) as u16;
                assert!(total > body.height);
                let mut complete = Buffer::empty(Rect::new(0, 0, body.width, total));
                paragraph.render(complete.area, &mut complete);
                let footer = |buffer: &Buffer| {
                    (pane.x..pane.right())
                        .map(|x| buffer[(x, pane.bottom() - 1)].symbol())
                        .collect::<String>()
                };
                assert!(footer(&initial).contains(MORE_BELOW));
                let mut reached = BTreeSet::new();
                for offset in 0..=total - body.height {
                    let buffer = render_size(&mut editor, width, HEIGHT, &theme);
                    for y in 0..body.height {
                        reached.insert(offset + y);
                        for x in 0..body.width {
                            assert_eq!(buffer[(body.x + x, body.y + y)], complete[(x, offset + y)]);
                        }
                    }
                    for y in other.y..other.bottom() {
                        for x in other.x..other.right() {
                            assert_eq!(buffer[(x, y)], initial[(x, y)]);
                        }
                    }
                    let current: Vec<_> = editor
                        .hits
                        .iter()
                        .filter(|hit| {
                            matches!(hit.control, Control::Change(_) | Control::Requirement(_))
                        })
                        .cloned()
                        .collect();
                    assert!(current == anchors);
                    for hit in &anchors {
                        for y in hit.area.y..hit.area.bottom() {
                            for x in hit.area.x..hit.area.right() {
                                assert_eq!(buffer[(x, y)], initial[(x, y)]);
                            }
                        }
                    }
                    assert!(editor.handle_key(KeyEvent::from(KeyCode::Down)).is_none());
                    assert!(!editor.confirmed);
                }
                assert_eq!(reached, (0..total).collect::<BTreeSet<_>>());
                let final_buffer = render_size(&mut editor, width, HEIGHT, &theme);
                assert!(footer(&final_buffer).contains(MORE_ABOVE));
                assert!(!footer(&final_buffer).contains(MORE_BELOW));
                assert!(editor.preview_seen);
            }
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn detail_mouse_navigation_is_independent_and_save_never_requires_paging(width: u16) {
        let theme = theme::load_by_name("ayu_dark").unwrap();
        let mut editor = authority_change_editor(true);
        let before = render_size(&mut editor, width, HEIGHT, &theme);
        let after_pane = change_pane(&editor, &Side::After);
        let before_pane = change_pane(&editor, &Side::Before);
        let list_offset = editor.offset;
        assert!(
            editor
                .handle_mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: before_pane.x,
                    row: before_pane.y + 1,
                    modifiers: KeyModifiers::NONE
                })
                .is_none()
        );
        let after = render_size(&mut editor, width, HEIGHT, &theme);
        assert_ne!(before, after);
        assert_eq!(editor.offset, list_offset);
        for y in after_pane.y..after_pane.bottom() {
            for x in after_pane.x..after_pane.right() {
                assert_eq!(before[(x, y)], after[(x, y)]);
            }
        }
        assert!(click(&mut editor, Control::ChangeDetail(DetailControl::Next)).is_none());
        assert!(editor.preview.is_some());
        assert!(!editor.confirmed);

        let mut editor = authority_change_editor(true);
        render_size(&mut editor, width, HEIGHT, &theme);
        assert!(
            click(
                &mut editor,
                Control::ChangeDetail(DetailControl::Pane(Side::Before))
            )
            .is_none()
        );
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(editor.activate(Control::Confirm).is_none());
        assert!(matches!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(EditorEvent::Save { .. })
        ));
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn every_typed_predicate_is_selectable_without_repositioning_changes(width: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let theme = theme::load_by_name(name).unwrap();
            let mut editor = authority_change_editor(false);
            let initial = render_size(&mut editor, width, HEIGHT, &theme);
            let count = editor.change_reviews()[0].1.predicates.len();
            let row = editor
                .hits
                .iter()
                .find(|hit| hit.control == Control::Change(0))
                .unwrap()
                .area;
            let mut panels = BTreeSet::new();
            for index in 0..count {
                let buffer = render_size(&mut editor, width, HEIGHT, &theme);
                let pane = change_pane(&editor, &Side::Before);
                let navigation: String = (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, pane.y - 2)].symbol())
                    .collect();
                assert!(navigation.contains(&format!("Detail {}/{}", index + 1, count)));
                let label_y = pane.y - 1;
                let label: String = (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, label_y)].symbol())
                    .collect();
                panels.insert(label);
                for x in row.x..row.right() {
                    assert_eq!(buffer[(x, row.y)], initial[(x, row.y)]);
                }
                assert!(click(&mut editor, Control::ChangeDetail(DetailControl::Next)).is_none());
            }
            assert!(panels.iter().any(|label| label.contains("Access")));
            assert!(panels.iter().any(|label| label.contains("Protection")));
            assert!(panels.iter().any(|label| label.contains("Combinations")));
            assert!(panels.iter().any(|label| label.contains("Domain")));
            assert!(editor.preview_seen);
            assert!(!editor.confirmed);
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn wheel_reaches_every_template_and_change_control(width: u16) {
        let theme = theme::load_by_name("ayu_dark").unwrap();
        for section in [Section::Targets, Section::Template, Section::Changes] {
            let mut editor = review_editor();
            editor.activate(Control::Section(section));
            let expected: Vec<_> = editor.rows().into_iter().map(|row| row.control).collect();
            let mut visible = Vec::new();
            for _ in 0..=expected.len() {
                render_size(&mut editor, width, HEIGHT, &theme);
                visible.extend(editor.hits.iter().map(|hit| hit.control.clone()));
                let list_row = editor
                    .hits
                    .iter()
                    .find(|hit| matches!(hit.control, Control::Change(_) | Control::Requirement(_)))
                    .map(|hit| hit.area);
                editor.handle_mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: list_row.map_or(0, |row| row.x),
                    row: list_row.map_or(HEIGHT / 2, |row| row.y),
                    modifiers: KeyModifiers::NONE,
                });
            }
            assert!(expected.iter().all(|control| visible.contains(control)));
        }
    }

    #[test_case(40; "narrow_manager")]
    #[test_case(80; "normal_manager")]
    #[test_case(140; "wide_manager")]
    fn manager_viewport_keeps_editor_focus_reachable(width: u16) {
        for section in [
            Section::Rule,
            Section::Targets,
            Section::Arguments,
            Section::Template,
            Section::Changes,
            Section::Test,
        ] {
            let mut editor = review_editor();
            editor.activate(Control::Section(section));
            let expected: Vec<_> = editor.rows().into_iter().map(|row| row.control).collect();
            let mut picker = PermissionsPicker::new();
            picker.open(vec![record(true)], &[], &[], false, false);
            picker.set_editor(editor);
            let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
            let mut visited = Vec::new();
            for _ in 0..MAX_FOCUS_STEPS {
                picker.handle_key(KeyEvent::from(KeyCode::Tab));
                terminal
                    .draw(|frame| {
                        picker.view(frame, frame.area());
                    })
                    .unwrap();
                let editor = picker.editor_mut().unwrap();
                assert!(
                    editor
                        .hits
                        .iter()
                        .any(|hit| hit.control == editor.focus && !hit.area.is_empty())
                );
                visited.push(editor.focus.clone());
                if expected.iter().all(|control| visited.contains(control)) {
                    break;
                }
            }
            assert!(expected.iter().all(|control| visited.contains(control)));
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn equivalent_preview_has_no_unrendered_detail_controls(width: u16) {
        let mut editor = editor();
        editor.receive_preview(editor.revision(), Ok(preview()));
        let theme = theme::load_by_name("ayu_dark").unwrap();
        for _ in 0..MAX_FOCUS_STEPS {
            assert!(editor.handle_key(KeyEvent::from(KeyCode::Tab)).is_none());
            render_size(&mut editor, width, HEIGHT, &theme);
            assert!(!matches!(editor.focus, Control::ChangeDetail(_)));
            assert!(editor.hits.iter().any(|hit| hit.control == editor.focus));
        }
        assert!(editor.preview_seen);
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn manager_wheel_routes_to_the_editor_pane_not_inventory(width: u16) {
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record(true)], &[], &[], false, false);
        picker.set_editor(authority_change_editor(true));
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let before = terminal.backend().buffer().clone();
        let pane = change_pane(picker.editor_mut().unwrap(), &Side::Before);
        picker.scroll_at(Position::new(pane.x, pane.y + 1), -1);
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let after = terminal.backend().buffer();
        assert_ne!(&before, after);
        for y in before.area.y..before.area.bottom() {
            for x in before.area.x..before.area.right() {
                if !pane.contains(Position::new(x, y)) {
                    assert_eq!(before[(x, y)], after[(x, y)]);
                }
            }
        }
        assert!(!picker.editor_mut().unwrap().confirmed);
    }

    #[test_case(80, 22; "manager_80x22")]
    #[test_case(40, 20; "manager_40x20")]
    fn short_manager_keeps_changes_full_values_and_confirmation_reachable(width: u16, height: u16) {
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record(true)], &[], &[], false, false);
        picker.set_editor(authority_change_editor(true));
        render_manager(&mut picker, width, height);
        assert!(picker.editor_mut().unwrap().preview_seen);
        for (key, expected) in [
            (KeyEvent::from(KeyCode::Tab), Control::Change(1)),
            (
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
                Control::Change(0),
            ),
        ] {
            assert!(picker.editor_mut().unwrap().handle_key(key).is_none());
            let buffer = render_manager(&mut picker, width, height);
            let editor = picker.editor_mut().unwrap();
            assert_eq!(editor.focus, expected);
            let hit = editor
                .hits
                .iter()
                .find(|hit| hit.control == expected)
                .unwrap();
            assert!(!hit.area.is_empty());
            assert_eq!(hit.area.intersection(buffer.area), hit.area);
        }
        for side in [Side::Before, Side::After] {
            assert!(
                picker
                    .editor_mut()
                    .unwrap()
                    .activate(Control::ChangeDetail(DetailControl::Pane(side.clone())))
                    .is_none()
            );
            let initial = render_manager(&mut picker, width, height);
            let editor = picker.editor_mut().unwrap();
            let pane = change_pane(editor, &side);
            let body = Rect::new(pane.x, pane.y + 1, pane.width, pane.height - 2);
            assert!(!body.is_empty());
            let other = change_pane(
                editor,
                &if side == Side::Before {
                    Side::After
                } else {
                    Side::Before
                },
            );
            let anchors: Vec<_> = editor
                .hits
                .iter()
                .filter(|hit| matches!(hit.control, Control::Change(_) | Control::Requirement(_)))
                .cloned()
                .collect();
            assert!(!anchors.is_empty());
            let (_, review) = editor.change_reviews().remove(0);
            let predicate = &review.predicates[0];
            let value = if side == Side::Before {
                &predicate.before
            } else {
                &predicate.after
            };
            let paragraph = Paragraph::new(vec![
                Line::from(value.clone()),
                Line::from(predicate.label.clone()),
            ])
            .wrap(Wrap { trim: false });
            let total = paragraph.line_count(body.width) as u16;
            assert!(total > body.height);
            let mut complete = Buffer::empty(Rect::new(0, 0, body.width, total));
            paragraph.render(complete.area, &mut complete);
            let mut reached = BTreeSet::new();
            for offset in 0..=total - body.height {
                let buffer = render_manager(&mut picker, width, height);
                for y in 0..body.height {
                    reached.insert(offset + y);
                    for x in 0..body.width {
                        assert_eq!(
                            buffer[(body.x + x, body.y + y)].symbol(),
                            complete[(x, offset + y)].symbol()
                        );
                    }
                }
                for y in other.y..other.bottom() {
                    for x in other.x..other.right() {
                        assert_eq!(buffer[(x, y)], initial[(x, y)]);
                    }
                }
                for hit in &anchors {
                    for y in hit.area.y..hit.area.bottom() {
                        for x in hit.area.x..hit.area.right() {
                            assert_eq!(buffer[(x, y)], initial[(x, y)]);
                        }
                    }
                }
                let editor = picker.editor_mut().unwrap();
                assert!(
                    editor
                        .hits
                        .iter()
                        .any(|hit| hit.control == Control::Confirm && !hit.area.is_empty())
                );
                assert!(editor.preview_seen);
                assert!(!editor.confirmed);
                assert!(editor.handle_key(KeyEvent::from(KeyCode::Down)).is_none());
            }
            assert_eq!(reached, (0..total).collect::<BTreeSet<_>>());
        }
        let editor = picker.editor_mut().unwrap();
        assert!(editor.activate(Control::Confirm).is_none());
        assert!(matches!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(EditorEvent::Save { .. })
        ));
    }

    #[test_case(40, 24; "manager_40x24")]
    #[test_case(40, 20; "manager_40x20")]
    #[test_case(80, 22; "manager_80x22")]
    fn target_predicate_columns_keep_a_gap_and_the_selected_value_readable(
        width: u16,
        height: u16,
    ) {
        let mut editor = registered_editor();
        editor.activate(Control::Section(Section::Targets));
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record(true)], &[], &[], false, false);
        picker.set_editor(editor);
        for _ in 0..MAX_FOCUS_STEPS {
            if picker.editor_mut().unwrap().focus == Control::Protection {
                break;
            }
            assert!(
                picker
                    .editor_mut()
                    .unwrap()
                    .handle_key(KeyEvent::from(KeyCode::Tab))
                    .is_none()
            );
            render_manager(&mut picker, width, height);
        }
        let buffer = render_manager(&mut picker, width, height);
        let editor = picker.editor_mut().unwrap();
        assert_eq!(editor.focus, Control::Protection);
        let area = editor
            .hits
            .iter()
            .find(|hit| hit.control == Control::Protection)
            .unwrap()
            .area;
        let row: String = (area.x..area.right())
            .map(|x| buffer[(x, area.y)].symbol())
            .collect();
        let label_end = row.find(PROTECTION_LABEL).unwrap() + PROTECTION_LABEL.len();
        let value_start = row.find(UNPROTECTED_ONLY).unwrap();
        assert!(value_start > label_end);
        assert!(row[label_end..value_start].chars().all(char::is_whitespace));
        assert!(row.trim_end().ends_with(UNPROTECTED_ONLY));
    }

    #[test]
    #[ignore = "private typed predicate and independent scroll exports; run alone and review before acceptance"]
    fn export_permission_change_details_visual_review() {
        let directory = visual_directory("caudra-permission-changes-");
        for name in ["ayu_dark", "ayu_light"] {
            theme::set(theme::load_by_name(name).unwrap());
            for (width, height) in [(40, 20), (80, 22), (40, 24), (80, 24), (140, 32)] {
                for manager in [false, true] {
                    for (case, editor) in [
                        ("expansion", authority_change_editor(false)),
                        ("long", authority_change_editor(true)),
                        ("arguments-exact", argument_change_editor(false)),
                        ("arguments-selected", argument_change_editor(true)),
                    ] {
                        let count = editor.change_reviews()[0]
                            .1
                            .predicates
                            .iter()
                            .filter(|predicate| predicate.changed)
                            .count();
                        let mut picker = PermissionsPicker::new();
                        picker.open(vec![record(true)], &[], &[], false, false);
                        picker.set_editor(editor);
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        for predicate in 0..count {
                            for stage in ["top", "before-end", "after-end"] {
                                if stage != "top" {
                                    let editor = picker.editor_mut().unwrap();
                                    editor.activate(Control::ChangeDetail(DetailControl::Pane(
                                        if stage == "before-end" {
                                            Side::Before
                                        } else {
                                            Side::After
                                        },
                                    )));
                                    editor.handle_key(KeyEvent::from(KeyCode::End));
                                }
                                terminal
                                    .draw(|frame| {
                                        if manager {
                                            picker.view(frame, frame.area());
                                        } else {
                                            picker.editor_mut().unwrap().view(frame, frame.area());
                                        }
                                    })
                                    .unwrap();
                                let stem = format!(
                                    "{name}-{width}x{height}-{}-{}-{predicate}-{stage}",
                                    if manager { "manager" } else { "editor" },
                                    case
                                );
                                write_visual_buffer(
                                    directory.path(),
                                    &stem,
                                    terminal.backend().buffer(),
                                );
                            }
                            picker
                                .editor_mut()
                                .unwrap()
                                .activate(Control::ChangeDetail(DetailControl::Next));
                        }
                    }
                }
            }
        }
        println!("{}", directory.keep().display());
    }

    #[test]
    #[ignore = "private read/edit/changes buffers; run alone with --test-threads=1 and review before acceptance"]
    fn export_scope_editor_visual_review() {
        let directory = visual_directory("caudra-scope-editor-");
        for name in ["ayu_dark", "ayu_light"] {
            theme::set(theme::load_by_name(name).unwrap());
            for (width, height) in [(40, 20), (80, 22), (40, 24), (80, 24), (140, 32)] {
                for manager in [false, true] {
                    for (panel, section, control) in [
                        ("read", Section::Scope, None),
                        ("rule", Section::Rule, None),
                        ("targets", Section::Targets, None),
                        ("target-protection", Section::Targets, None),
                        ("arguments", Section::Arguments, None),
                        ("template", Section::Template, None),
                        (
                            "structure",
                            Section::Template,
                            Some(Control::StructureLiteral),
                        ),
                        (
                            "tuples",
                            Section::Template,
                            Some(Control::TupleValue(PATH_SLOT)),
                        ),
                        ("edit", Section::Rule, Some(Control::Label)),
                        ("test", Section::Test, Some(Control::TestInput)),
                        ("changes", Section::Changes, Some(Control::Change(0))),
                        ("change-long", Section::Changes, Some(Control::Change(0))),
                        (
                            "change-expansion",
                            Section::Changes,
                            Some(Control::Change(0)),
                        ),
                        (
                            "requirements",
                            Section::Changes,
                            Some(Control::Requirement(
                                ConfirmationRequirement::MayReleasePendingRequests,
                            )),
                        ),
                    ] {
                        let mut editor = match panel {
                            "change-long" => authority_change_editor(true),
                            "change-expansion" => authority_change_editor(false),
                            _ => review_editor(),
                        };
                        editor.activate(Control::Section(section));
                        if let Some(control) = control {
                            editor.activate(control);
                        }
                        match panel {
                            "target-protection" => { editor.focus = Control::Protection; editor.reveal_focus = true; }
                            "edit" => editor.handle_paste(LABEL),
                            "structure" => editor.handle_paste(MANUAL),
                            "test" => editor.handle_paste(&serde_json::json!({"command": ANALYSIS_COMMAND, "workdir": WORKDIR}).to_string()),
                            _ => {}
                        }
                        let mut picker = PermissionsPicker::new();
                        picker.open(vec![record(true)], &[], &[], false, false);
                        picker.set_editor(editor);
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal
                            .draw(|frame| {
                                if manager {
                                    picker.view(frame, frame.area());
                                } else {
                                    picker.editor_mut().unwrap().view(frame, frame.area());
                                }
                            })
                            .unwrap();
                        let stem = format!(
                            "{name}-{width}x{height}-{}-{panel}",
                            if manager { "manager" } else { "editor" }
                        );
                        write_visual_buffer(directory.path(), &stem, terminal.backend().buffer());
                    }
                }
            }
        }
        println!("{}", directory.keep().display());
    }
}
