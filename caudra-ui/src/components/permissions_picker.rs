use caudra_agent::permissions::{
    ActivePolicyRule, PermissionArgumentConstraint, PermissionLifetime, PermissionResourceKind,
    PermissionResourceSelector, PermissionReviewSource, PermissionRuleRecord, PermissionSubject,
    StructuredPermissionEffect, VerifiedLocalSourceLocator,
    pattern_recognition::{MAX_RECOGNIZER_SUGGESTIONS, PatternCandidate, RecognizerLimits},
};
use caudra_config::{
    Effect, PermissionReviewCandidate, PermissionReviewKind, PermissionRule, PermissionSource,
};
use caudra_storage::permission_patterns::{
    ArgumentDomain, ObservedTuple, OptionLikePolicy, PatternDefinition, PatternToken,
    SlotCombinations, SlotId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

use crate::PatternDiscoveryOutcome;
use crate::components::keybindings::Bind;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::{CHROME_LINES, FooterHits, FooterLine, Modal};
use crate::components::permission_scope::editor::{EditorEvent, EditorLaunch, ScopeEditor};
use crate::components::permission_scope::{
    model::{ScopeActivity, ScopeModel, rule_kind},
    view::ScopeView,
};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{Hint, HintBar, ModalScroll, Overlay, escape_terminal_controls, hint_line};
use crate::theme;

const TITLE: &str = " Permissions ";
/// The picker's own chords, spelled the way its toolbar already spells them.
const DISMISS_SUGGESTION: Bind = Bind {
    code: KeyCode::Char('d'),
    modifiers: KeyModifiers::CONTROL,
    label: "^D",
};
const SNOOZE_SUGGESTION: Bind = Bind {
    code: KeyCode::Char('s'),
    modifiers: KeyModifiers::CONTROL,
    label: "^S",
};
const REFRESH_DISCOVERY: Bind = Bind {
    code: KeyCode::Char('r'),
    modifiers: KeyModifiers::CONTROL,
    label: "^R",
};
const EMPTY: &str = "No grants or policies. Use Discover to scan saved history.";
const EMPTY_DISCOVERY: &str = "No visible proposals. See the scan overview.";
const RULES_TITLE: &str = " Rules · grants & policies ";
const DISCOVER_TITLE: &str = " Discover · not active ";
const MAX_DISPLAY_CHARS: usize = 2048;
const MAX_FIELD_CHARS: usize = 256;
const MAX_DISPLAY_ITEMS: usize = 16;
const MAX_INPUT_DEPTH: usize = 4;
const UNAVAILABLE: &str = "unavailable";
const OMITTED: &str = "[omitted: display limit]";
const SUGGESTED_SECTION: &str = "Suggested · not active";
const SUGGESTED_TITLE: &str = " Proposal evidence · not active ";
const SUGGESTED_GUIDANCE: &str = "Create permission opens a draft for current-identity validation. Evidence alone grants nothing.";
const SUGGESTED_DISMISSAL_HELP: &str =
    "Dismiss: this definition in this project. Snooze: hide it in this project for 24 hours.";
const MAX_SUGGESTION_EXAMPLES: usize = 3;
const MAX_SUGGESTION_DETAIL_CHARS: usize = 8192;
const INSPECTOR_WIDTH_PERCENT: u16 = 80;
const INSPECTOR_HEIGHT_PERCENT: u16 = 80;
const SCROLLBAR_WIDTH: u16 = 1;
const MANAGER_SIZE_PERCENT: u16 = 95;
const SIDE_BY_SIDE_WIDTH: u16 = 110;
const LIST_WIDTH: u16 = 56;
const PANE_GAP: u16 = 1;
const MIN_CHROME_HEIGHT: u16 = 16;
const DISCOVERY_WARNING: &str = "Imported history is unverified. Standard Bash startup and tool identity are assumed; current stored cwd approximates historical context. Execution success is not proven.";
const DISCOVERY_EMPTY: &str = "No visible proposals. Unsupported or sensitive commands are excluded; dismissed and snoozed definitions stay hidden. A bounded sample can miss otherwise eligible patterns.";
const DISCOVERY_IDLE: &str = "Scan local saved history for suggested command patterns. Nothing is installed or approved by a scan.";
const DISCOVERY_LOADING: &str = "Reading and analyzing a bounded history sample in the background. Cancel stops this request; active permissions are unchanged.";
const DISCOVERY_CANCELLED: &str =
    "Scan cancelled. No late result from this request will be installed. Refresh to scan again.";
const READ_ONLY_POLICY: &str = "Read-only: no verified local source locator or supported editing API. No saved override is created.";
const INACTIVE_ALLOW: &str = "This legacy allow is inactive. Re-approve the next exact request or remove the old config entry.";
const CONFIRM_REVOKE_MESSAGE: &str =
    "Revoke this permission? Press Enter/y to confirm or Esc to cancel.";

pub(crate) enum PermissionsPickerAction {
    Consumed,
    Close,
    Revoke(String),
    TrustProjectConfig,
    RevokeProjectConfigTrust,
    DismissSuggestion(SuggestedPatternTarget),
    SnoozeSuggestion(SuggestedPatternTarget),
    RefreshDiscovery,
    CancelDiscovery,
    Editor(EditorEvent),
    EditSource(VerifiedLocalSourceLocator),
}

pub(crate) enum DiscoveryState {
    Idle,
    Loading,
    Cancelled,
    Complete(Arc<PatternDiscoveryOutcome>),
}

#[derive(Debug, PartialEq, Eq)]
enum PermissionsMode {
    Rules,
    Discover,
}

#[derive(Default)]
enum ProjectFilter {
    #[default]
    All,
    Here,
    Other,
    History,
}

impl ProjectFilter {
    fn label(&self) -> &'static str {
        match self {
            Self::All => "[All ^F]",
            Self::Here => "[Here ^F]",
            Self::Other => "[Other ^F]",
            Self::History => "[History ^F]",
        }
    }
}

impl PermissionsMode {
    fn title(&self) -> &'static str {
        match self {
            Self::Rules => RULES_TITLE,
            Self::Discover => DISCOVER_TITLE,
        }
    }
}

impl DiscoveryState {
    fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Not scanned",
            Self::Loading => "Loading",
            Self::Cancelled => "Cancelled",
            Self::Complete(outcome) => match outcome.as_ref() {
                PatternDiscoveryOutcome::Ready(report) if !report.partial_reasons.is_empty() => {
                    "Partial"
                }
                PatternDiscoveryOutcome::Ready(_) => "Ready",
                PatternDiscoveryOutcome::Unavailable(_) => "Unavailable",
            },
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SuggestedPatternTarget {
    pub(crate) project: PathBuf,
    pub(crate) revision: u64,
    pub(crate) definition_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectConfigAction {
    Trust,
    RevokeTrust,
}

#[derive(Clone, PartialEq)]
enum PickerEntry {
    Stored(Arc<PermissionRuleRecord>),
    Discovered(Arc<PatternCandidate>),
    Policy {
        source: &'static str,
        rule: PermissionRule,
        locator: Option<VerifiedLocalSourceLocator>,
    },
    Review(PermissionReviewCandidate),
    ProjectConfig,
}

#[derive(Clone, PartialEq)]
struct PermissionEntry {
    source: PickerEntry,
    id: Option<String>,
    tool: String,
    detail: String,
    description: Option<String>,
    read_only_policy: bool,
    project_config_action: Option<ProjectConfigAction>,
    suggestion: Option<SuggestedPatternTarget>,
}

impl PermissionEntry {
    fn scope(&self) -> Option<ScopeModel> {
        match &self.source {
            PickerEntry::Stored(record) => Some(ScopeModel::record(record.clone())),
            PickerEntry::Discovered(candidate) => Some(ScopeModel::candidate(candidate.clone())),
            _ => None,
        }
    }
}

impl PickerItem for PermissionEntry {
    fn label(&self) -> &str {
        &self.tool
    }

    fn section(&self) -> Option<&str> {
        Some(if self.suggestion.is_some() {
            SUGGESTED_SECTION
        } else if self.project_config_action.is_some() {
            "Project configuration"
        } else if self.id.is_some() {
            "Stored permissions"
        } else if self.read_only_policy {
            "Active policy · read-only"
        } else {
            "Needs review · inactive"
        })
    }
}

pub(crate) struct PermissionsPicker {
    picker: ListPicker<PermissionEntry>,
    entries: Vec<PermissionEntry>,
    pending_revoke: Option<String>,
    pending_project_config_action: Option<ProjectConfigAction>,
    suggestion_inspector: Option<SuggestionInspector>,
    discovery: DiscoveryState,
    mode: PermissionsMode,
    other_selection: usize,
    discovery_view: bool,
    detail_focused: bool,
    detail: SuggestionInspector,
    notice: Option<String>,
    popup: Rect,
    toolbar_hits: FooterHits,
    tabs_hits: FooterHits,
    footer: HintBar,
    scope_view: ScopeView,
    editor: Option<ScopeEditor>,
    current_project: Option<PathBuf>,
    project_filter: ProjectFilter,
}

struct SuggestionInspector {
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    popup: Rect,
}

impl SuggestionInspector {
    fn new() -> Self {
        Self {
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            popup: Rect::default(),
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return true,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return true;
            }
        }
        if self.popup.contains(Position::new(event.column, event.row)) {
            match event.kind {
                MouseEventKind::ScrollUp => self.scroll.scroll(1),
                MouseEventKind::ScrollDown => self.scroll.scroll(-1),
                _ => return false,
            }
            return true;
        }
        false
    }

    fn view(&mut self, frame: &mut Frame, area: Rect, description: &str) -> Rect {
        let mut lines = vec![Line::from(SUGGESTED_GUIDANCE), Line::default()];
        lines.extend(description.lines().map(Line::from));
        lines.extend([
            Line::default(),
            Line::from(SUGGESTED_DISMISSAL_HELP),
            suggestion_inspector_footer(),
        ]);
        let paragraph = Paragraph::new(lines)
            .style(theme::current().item)
            .wrap(Wrap { trim: false });
        let width = Modal::inner_width(area.width, INSPECTOR_WIDTH_PERCENT)
            .saturating_sub(SCROLLBAR_WIDTH)
            .max(1);
        let total = u16::try_from(paragraph.line_count(width))
            .unwrap_or(u16::MAX)
            .min(u16::MAX.saturating_sub(CHROME_LINES));
        let (popup, inner) = Modal {
            title: SUGGESTED_TITLE,
            width_percent: INSPECTOR_WIDTH_PERCENT,
            max_height_percent: INSPECTOR_HEIGHT_PERCENT,
        }
        .render(frame, area, total);
        let body = Rect {
            width: inner.width.saturating_sub(SCROLLBAR_WIDTH),
            ..inner
        };
        self.scroll.update_dimensions(total, body.height);
        let offset = self.scroll.offset();
        frame.render_widget(paragraph.scroll((offset, 0)), body);
        self.scrollbar.draw(frame, inner, total, offset);
        self.popup = popup;
        popup
    }
}

impl PermissionsPicker {
    pub(crate) fn new() -> Self {
        let mut picker = ListPicker::new().with_width_percent(100);
        picker.set_empty_text(EMPTY);
        picker.set_footer_builder(footer);
        Self {
            picker,
            entries: Vec::new(),
            pending_revoke: None,
            pending_project_config_action: None,
            suggestion_inspector: None,
            discovery: DiscoveryState::Idle,
            mode: PermissionsMode::Rules,
            other_selection: 0,
            discovery_view: false,
            detail_focused: false,
            detail: SuggestionInspector::new(),
            notice: None,
            popup: Rect::default(),
            toolbar_hits: FooterHits::default(),
            tabs_hits: FooterHits::default(),
            footer: HintBar::default(),
            scope_view: ScopeView::default(),
            editor: None,
            current_project: None,
            project_filter: ProjectFilter::All,
        }
    }

    pub(crate) fn open(
        &mut self,
        rules: Vec<PermissionRuleRecord>,
        review_candidates: &[PermissionReviewCandidate],
        effective_policy: &[ActivePolicyRule],
        needs_project_config_trust: bool,
        project_config_trusted: bool,
    ) {
        self.entries = needs_project_config_trust
            .then(|| project_config_entry(ProjectConfigAction::Trust))
            .or_else(|| {
                project_config_trusted
                    .then(|| project_config_entry(ProjectConfigAction::RevokeTrust))
            })
            .into_iter()
            .chain(rules.into_iter().map(entry))
            .chain(review_candidates.iter().map(review_entry))
            .chain(effective_policy.iter().map(policy_entry))
            .collect();
        self.pending_revoke = None;
        self.pending_project_config_action = None;
        self.suggestion_inspector = None;
        self.notice = None;
        self.detail = SuggestionInspector::new();
        self.detail_focused = false;
        self.mode = PermissionsMode::Rules;
        self.other_selection = 0;
        self.discovery_view = false;
        self.picker.set_empty_text(EMPTY);
        self.picker
            .set_footer_builder(if needs_project_config_trust || project_config_trusted {
                trust_footer
            } else {
                footer
            });
        self.picker.open(self.visible_entries(), self.mode.title());
    }

    pub(crate) fn set_discovery(&mut self, state: DiscoveryState) {
        self.discovery = state;
        if self.discovery_view {
            self.detail.scroll.reset();
        }
    }

    pub(crate) fn discovery_cancelled(&self) -> bool {
        matches!(self.discovery, DiscoveryState::Cancelled)
    }

    #[cfg(test)]
    pub(crate) fn discovery_state(&self) -> &DiscoveryState {
        &self.discovery
    }

    #[cfg(test)]
    pub(crate) fn discovery_mode(&self) -> bool {
        self.mode == PermissionsMode::Discover
    }

    pub(crate) fn show_discovery(&mut self) {
        self.set_mode(PermissionsMode::Discover);
    }

    fn visible_entries(&self) -> Vec<PermissionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.suggestion.is_some() == (self.mode == PermissionsMode::Discover))
            .filter(|entry| match (&self.project_filter, &entry.source) {
                (ProjectFilter::All, _) => true,
                (ProjectFilter::History, PickerEntry::Stored(record)) => {
                    record.revoked_at.is_some()
                }
                (ProjectFilter::Here, PickerEntry::Stored(record)) => {
                    record.revoked_at.is_none()
                        && self.current_project.as_ref().is_some_and(|project| {
                            record
                                .project
                                .as_ref()
                                .is_none_or(|binding| binding == project)
                        })
                }
                (ProjectFilter::Other, PickerEntry::Stored(record)) => record
                    .project
                    .as_ref()
                    .is_some_and(|binding| Some(binding) != self.current_project.as_ref()),
                (_, PickerEntry::Discovered(_)) => true,
                _ => false,
            })
            .cloned()
            .collect()
    }

    fn set_mode(&mut self, mode: PermissionsMode) {
        if self.has_pending_confirmation() || self.mode == mode {
            return;
        }
        let selected = self.picker.selected_index().unwrap_or_default();
        self.mode = mode;
        self.picker
            .set_empty_text(if self.mode == PermissionsMode::Discover {
                EMPTY_DISCOVERY
            } else {
                EMPTY
            });
        self.picker.open(self.visible_entries(), self.mode.title());
        self.picker.select(self.other_selection);
        self.other_selection = selected;
        self.discovery_view = false;
        self.detail_focused = false;
        self.detail.scroll.reset();
        self.suggestion_inspector = None;
        self.notice = None;
        self.tabs_hits.clear();
        self.toolbar_hits.clear();
    }

    fn show_discovery_overview(&mut self) {
        self.show_discovery();
        self.discovery_view = true;
        self.detail_focused = true;
        self.detail.scroll.reset();
        self.notice = None;
    }

    pub(crate) fn set_suggestions(
        &mut self,
        project: &Path,
        revision: u64,
        candidates: &[PatternCandidate],
    ) {
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.suggestion.is_none())
            .cloned()
            .chain(
                candidates
                    .iter()
                    .take(MAX_RECOGNIZER_SUGGESTIONS)
                    .filter_map(|candidate| suggestion_entry(project, revision, candidate)),
            )
            .collect();
        if entries == self.entries {
            return;
        }
        let selected = self.picker.selected_item().cloned();
        self.entries = entries;
        if self.mode == PermissionsMode::Rules {
            self.other_selection = 0;
            return;
        }
        self.picker.replace_items(self.visible_entries());
        let restored = selected.is_some_and(|selected| {
            self.picker.select_item_by(|entry| {
                if let Some(target) = &selected.suggestion {
                    entry.suggestion.as_ref() == Some(target)
                } else {
                    entry == &selected
                }
            })
        });
        if self.suggestion_inspector.is_some() {
            if restored {
                self.inspect_suggestion();
            } else {
                self.suggestion_inspector = None;
            }
        }
        if !restored {
            self.picker.select(0);
            self.detail.scroll.reset();
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PermissionsPickerAction {
        if let Some(editor) = &mut self.editor {
            return editor.handle_key(key).map_or(
                PermissionsPickerAction::Consumed,
                PermissionsPickerAction::Editor,
            );
        }
        if key.kind != crossterm::event::KeyEventKind::Press {
            return PermissionsPickerAction::Consumed;
        }
        if !self.has_pending_confirmation()
            && !self.discovery_view
            && key.modifiers == KeyModifiers::CONTROL
            && let Some(target) = self
                .picker
                .selected_item()
                .and_then(|entry| entry.suggestion.clone())
        {
            match key.code {
                KeyCode::Char('d') => return PermissionsPickerAction::DismissSuggestion(target),
                KeyCode::Char('s') => return PermissionsPickerAction::SnoozeSuggestion(target),
                _ => {}
            }
        }
        if let Some(inspector) = &mut self.suggestion_inspector {
            if key.code == KeyCode::Esc || key::QUIT.matches(key) {
                self.suggestion_inspector = None;
            } else {
                inspector.scroll.handle_key(key);
            }
            return PermissionsPickerAction::Consumed;
        }
        if let Some(action) = self.pending_project_config_action {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.pending_project_config_action = None;
                    match action {
                        ProjectConfigAction::Trust => PermissionsPickerAction::TrustProjectConfig,
                        ProjectConfigAction::RevokeTrust => {
                            PermissionsPickerAction::RevokeProjectConfigTrust
                        }
                    }
                }
                KeyCode::Esc => {
                    self.pending_project_config_action = None;
                    self.notice = None;
                    PermissionsPickerAction::Consumed
                }
                _ => PermissionsPickerAction::Consumed,
            };
        }
        if let Some(id) = self.pending_revoke.clone() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.pending_revoke = None;
                    PermissionsPickerAction::Revoke(id)
                }
                KeyCode::Esc => {
                    self.pending_revoke = None;
                    self.notice = None;
                    PermissionsPickerAction::Consumed
                }
                _ => PermissionsPickerAction::Consumed,
            };
        }
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('n') => return self.manage(EditorLaunch::New),
                KeyCode::Char('e') => return self.edit_selected(false),
                KeyCode::Char('u') => return self.edit_selected(true),
                KeyCode::Char('b') => return self.copy_selected(),
                KeyCode::Char('f') => {
                    self.cycle_project_filter();
                    return PermissionsPickerAction::Consumed;
                }
                KeyCode::Char('i') => {
                    self.inspect_suggestion();
                    return PermissionsPickerAction::Consumed;
                }
                KeyCode::Char('k') => {
                    if let Some(id) = self
                        .picker
                        .selected_item()
                        .and_then(|entry| entry.id.clone())
                    {
                        self.confirm_revoke(id);
                        self.detail_focused = true;
                    }
                    return PermissionsPickerAction::Consumed;
                }
                KeyCode::Char('r') => return self.discovery_action(false),
                KeyCode::Char('x') => return self.discovery_action(true),
                KeyCode::Char('g') => {
                    self.set_mode(if self.mode == PermissionsMode::Rules {
                        PermissionsMode::Discover
                    } else {
                        PermissionsMode::Rules
                    });
                    return PermissionsPickerAction::Consumed;
                }
                KeyCode::Char('o') => {
                    self.show_discovery_overview();
                    return PermissionsPickerAction::Consumed;
                }
                _ => {}
            }
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.detail_focused = !self.detail_focused;
            if !self.detail_focused && self.picker.selected_item().is_some() {
                self.discovery_view = false;
                self.detail.scroll.reset();
            }
            return PermissionsPickerAction::Consumed;
        }
        if self.detail_focused {
            if key.code == KeyCode::Esc {
                self.detail_focused = false;
            } else if key.code == KeyCode::Enter
                && self.mode == PermissionsMode::Discover
                && !self.discovery_view
            {
                self.scope_view.handle_key(key);
            } else if key::QUIT.matches(key) {
                return PermissionsPickerAction::Close;
            } else {
                if self
                    .picker
                    .selected_item()
                    .and_then(PermissionEntry::scope)
                    .is_some()
                {
                    self.scope_view.handle_key(key);
                } else {
                    self.detail.scroll.handle_key(key);
                }
            }
            return PermissionsPickerAction::Consumed;
        }
        if key.code == KeyCode::Enter {
            if let Some(entry) = self.picker.selected_item() {
                if let Some(action) = entry.project_config_action {
                    self.confirm_project_config_action(action);
                } else {
                    self.detail_focused = true;
                    self.discovery_view = false;
                }
            }
            return PermissionsPickerAction::Consumed;
        }
        if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
        ) && self.discovery_view
            && self.picker.selected_item().is_some()
        {
            self.discovery_view = false;
            self.detail.scroll.reset();
        }
        let selected = self.picker.selected_index();
        let action = self.picker.handle_key(key);
        self.selection_changed(selected);
        self.map_action(action)
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> PermissionsPickerAction {
        if let Some(editor) = &mut self.editor {
            return editor.handle_mouse(event).map_or(
                PermissionsPickerAction::Consumed,
                PermissionsPickerAction::Editor,
            );
        }
        if let Some(inspector) = &mut self.suggestion_inspector {
            inspector.handle_mouse(event);
            return PermissionsPickerAction::Consumed;
        }
        // Ahead of the confirmation gate: the hint is a key press, and the
        // key path already knows what Esc means while a confirmation is up.
        if let Some(key) = self.footer.handle_mouse(event) {
            return self.handle_key(key);
        }
        if self.has_pending_confirmation() {
            return PermissionsPickerAction::Consumed;
        }
        if self.notice.is_none()
            && !self.discovery_view
            && self
                .detail
                .popup
                .contains(Position::new(event.column, event.row))
            && self
                .picker
                .selected_item()
                .and_then(PermissionEntry::scope)
                .is_some()
            && self.scope_view.handle_mouse(event)
        {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.detail_focused = true;
            }
            return PermissionsPickerAction::Consumed;
        }
        if self.detail.handle_mouse(event) {
            return PermissionsPickerAction::Consumed;
        }
        if let Some(tab) = self.tabs_hits.handle_mouse(event) {
            if tab == 2 {
                self.cycle_project_filter();
                return PermissionsPickerAction::Consumed;
            }
            self.set_mode(if tab == 0 {
                PermissionsMode::Rules
            } else {
                PermissionsMode::Discover
            });
            return PermissionsPickerAction::Consumed;
        }
        if let Some(control) = self.toolbar_hits.handle_mouse(event) {
            if self.mode == PermissionsMode::Rules {
                return match control {
                    0 => self.manage(EditorLaunch::New),
                    1 => self.edit_selected(false),
                    2 => self.edit_selected(true),
                    4 => self.copy_selected(),
                    3 => {
                        if let Some(id) = self
                            .picker
                            .selected_item()
                            .and_then(|entry| entry.id.clone())
                        {
                            self.confirm_revoke(id);
                            self.detail_focused = true;
                        }
                        PermissionsPickerAction::Consumed
                    }
                    _ => PermissionsPickerAction::Consumed,
                };
            }
            match control {
                0 => return self.discovery_action(false),
                1 => return self.discovery_action(true),
                2 => self.show_discovery_overview(),
                3 => return self.edit_selected(false),
                _ => {}
            }
            return PermissionsPickerAction::Consumed;
        }
        let position = Position::new(event.column, event.row);
        if self.detail.popup.contains(position) {
            self.scope_view.handle_mouse(event);
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.detail_focused = true;
            }
            return PermissionsPickerAction::Consumed;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) && self.picker.contains(position) {
            self.detail_focused = false;
        }
        let selected = self.picker.selected_index();
        let action = self.picker.handle_mouse(event);
        self.selection_changed(selected);
        self.map_action(action)
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        if let Some(editor) = &mut self.editor {
            editor.handle_paste(text);
            return true;
        }
        if self.suggestion_inspector.is_some() || self.detail_focused {
            return true;
        }
        if self.has_pending_confirmation() {
            return true;
        }
        let selected = self.picker.selected_index();
        let consumed = self.picker.handle_paste(text);
        self.selection_changed(selected);
        consumed
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        if let Some(editor) = &mut self.editor {
            editor.scroll(delta);
            return;
        }
        if let Some(inspector) = &mut self.suggestion_inspector {
            inspector.scroll.scroll(delta);
            return;
        }
        if self.has_pending_confirmation() {
            return;
        }
        if self.detail_focused {
            self.scope_view.scroll(delta);
            self.detail.scroll.scroll(delta);
        } else {
            let selected = self.picker.selected_index();
            self.picker.scroll(delta);
            self.selection_changed(selected);
        }
    }

    pub(crate) fn scroll_at(&mut self, position: Position, delta: i32) {
        if let Some(editor) = &mut self.editor {
            editor.scroll_at(position, delta);
            return;
        }
        if self.suggestion_inspector.is_some() || self.has_pending_confirmation() {
            self.scroll(delta);
        } else if self.detail.popup.contains(position) {
            self.scope_view.scroll(delta);
            self.detail.scroll.scroll(delta);
        } else if self.picker.contains(position) {
            let selected = self.picker.selected_index();
            self.picker.scroll(delta);
            self.selection_changed(selected);
        }
    }

    pub(crate) fn contains(&self, position: Position) -> bool {
        if let Some(inspector) = &self.suggestion_inspector {
            inspector.popup.contains(position)
        } else {
            self.popup.contains(position)
        }
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if let Some(inspector) = &mut self.suggestion_inspector {
            let description = self
                .picker
                .selected_item()
                .and_then(|entry| entry.description.as_deref())
                .unwrap_or(UNAVAILABLE);
            return inspector.view(frame, area, description);
        }
        if !self.has_pending_confirmation() {
            let selected = self.picker.selected_item();
            let suggested = selected.is_some_and(|entry| entry.suggestion.is_some());
            let config_action = selected.is_some_and(|entry| entry.project_config_action.is_some());
            self.picker.set_footer_builder(if suggested {
                suggestion_footer
            } else if self.mode == PermissionsMode::Discover {
                discovery_footer
            } else if config_action {
                trust_footer
            } else {
                footer
            });
        }
        let title = format!("{}·{}", TITLE, self.mode.title());
        let (popup, inner) = Modal {
            title: &title,
            width_percent: MANAGER_SIZE_PERCENT,
            max_height_percent: MANAGER_SIZE_PERCENT,
        }
        .render(frame, area, area.height.saturating_sub(CHROME_LINES));
        if self.popup != popup {
            self.toolbar_hits.clear();
            self.tabs_hits.clear();
        }
        self.popup = popup;
        let actions: &[&str] = if self.editor.is_some() {
            &[]
        } else if self.mode == PermissionsMode::Rules {
            if self.picker.selected_item().is_some_and(|entry| {
                matches!(
                    &entry.source,
                    PickerEntry::Policy {
                        locator: Some(_),
                        ..
                    }
                )
            }) {
                &[
                    "[New ^N]",
                    "[Edit source ^E]",
                    "[Duplicate ^U]",
                    "[Revoke ^K]",
                    "[Copy ^B]",
                ]
            } else {
                &[
                    "[New ^N]",
                    "[Edit ^E]",
                    "[Duplicate ^U]",
                    "[Revoke ^K]",
                    "[Copy ^B]",
                ]
            }
        } else {
            &[
                "[Refresh ^R]",
                "[Cancel ^X]",
                "[Overview ^O]",
                "[Create permission ^E]",
            ]
        };
        let (_, toolbar_rows) = actions.iter().fold((0, 1), |(mut x, mut rows), action| {
            let width = (action.width() as u16).min(inner.width);
            if x + width > inner.width {
                x = 0;
                rows += 1;
            }
            (x + width + 1, rows)
        });
        let chrome = u16::from(area.height >= MIN_CHROME_HEIGHT);
        let [tabs, toolbar, status, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(toolbar_rows),
            Constraint::Length(chrome),
            Constraint::Min(0),
            Constraint::Length(chrome),
        ])
        .areas(inner);
        let theme = theme::current();
        let mut navigation = FooterLine::default();
        for (index, (label, mode)) in [
            ("[Rules]", PermissionsMode::Rules),
            ("[Discover]", PermissionsMode::Discover),
        ]
        .into_iter()
        .enumerate()
        {
            if index > 0 {
                navigation.text(" ", theme.item);
            }
            navigation.command(
                label,
                if self.mode == mode {
                    theme.item_selected
                } else {
                    theme.item
                },
            );
        }
        if self.mode == PermissionsMode::Rules {
            navigation.text(" ", theme.item);
            navigation.command(self.project_filter.label(), theme.item);
        }
        frame.render_widget(
            Paragraph::new(navigation.line(self.tabs_hits.hovered())),
            tabs,
        );
        self.tabs_hits.set(navigation.hits(tabs, 0, 1));
        let proposals = self
            .entries
            .iter()
            .filter(|entry| entry.suggestion.is_some())
            .count();
        let mut status_line = vec![Span::styled(
            if self.mode == PermissionsMode::Discover {
                format!(
                    "Not active · {} · {proposals} proposals",
                    self.discovery.label()
                )
            } else {
                format!("Grants & policies · Discovery: {}", self.discovery.label())
            },
            theme.panel_title,
        )];
        if let DiscoveryState::Complete(outcome) = &self.discovery
            && let PatternDiscoveryOutcome::Ready(report) = outcome.as_ref()
        {
            status_line.push(Span::styled(
                format!(
                    " · {} sessions · {} observations",
                    report.sample.sessions, report.recognition.retained_observations
                ),
                theme.item_desc,
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(status_line)), status);
        if self.editor.is_some() {
            frame.render_widget(
                Paragraph::new("Draft open · Save or Cancel below; inventory unchanged")
                    .style(theme.item_desc),
                toolbar,
            );
        }
        let (mut x, mut y) = (toolbar.x, toolbar.y);
        let mut action_hits = Vec::new();
        for action in actions {
            let width = (action.width() as u16).min(toolbar.width);
            if x + width > toolbar.right() {
                x = toolbar.x;
                y += 1;
            }
            if y >= toolbar.bottom() {
                break;
            }
            let cell = Rect::new(x, y, width.min(toolbar.width), 1);
            frame.render_widget(Paragraph::new(*action).style(theme.keybind_key), cell);
            action_hits.push(cell);
            x += width + 1;
        }
        self.toolbar_hits.set(action_hits);
        let [list, detail] = if body.width < SIDE_BY_SIDE_WIDTH {
            if self.detail_focused || self.has_pending_confirmation() || self.editor.is_some() {
                [Rect::default(), body]
            } else {
                [body, Rect::default()]
            }
        } else {
            let [list, _, detail] = Layout::horizontal([
                Constraint::Length(LIST_WIDTH),
                Constraint::Length(PANE_GAP),
                Constraint::Min(0),
            ])
            .areas(body);
            [list, detail]
        };
        if list.height > 0 {
            self.picker.view(frame, list);
        }
        if let Some(editor) = &mut self.editor {
            editor.view(frame, detail);
        } else {
            self.view_detail(frame, detail);
        }
        self.footer.draw(
            frame,
            footer,
            vec![
                Hint::bind(key::TAB, "List/Detail"),
                Hint::inert("PgUp/Dn", "Scroll"),
                Hint::bind(key::ESC, "Back"),
            ],
        );
        popup
    }

    fn view_detail(&mut self, frame: &mut Frame, area: Rect) {
        self.detail.popup = area;
        if area.is_empty() {
            return;
        }
        if self.notice.is_none()
            && !self.discovery_view
            && let Some(mut model) = self.picker.selected_item().and_then(PermissionEntry::scope)
        {
            if let Some(current) = &self.current_project
                && let Some(PickerEntry::Stored(record)) =
                    self.picker.selected_item().map(|entry| &entry.source)
                && record
                    .project
                    .as_ref()
                    .is_some_and(|project| project != current)
                && record.revoked_at.is_none()
            {
                model.activity = ScopeActivity::OtherProject;
            }
            self.scope_view
                .render(&model, area, frame.buffer_mut(), &theme::current());
            return;
        }
        let theme = theme::current();
        let block = Block::bordered()
            .title(if self.mode == PermissionsMode::Discover {
                if self.discovery_view || self.picker.selected_item().is_none() {
                    " Scan overview "
                } else {
                    " Proposal · not active "
                }
            } else {
                " Selected permission "
            })
            .border_style(if self.detail_focused {
                theme.panel_title
            } else {
                theme.panel_border
            });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = if let Some(notice) = &self.notice {
            vec![Line::styled(notice.clone(), theme.panel_title)]
        } else if self.mode == PermissionsMode::Discover
            && (self.discovery_view || self.picker.selected_item().is_none())
        {
            self.discovery_lines()
        } else if let Some(entry) = self.picker.selected_item() {
            let mut lines = if entry.suggestion.is_some() {
                Vec::new()
            } else {
                vec![Line::styled(entry.detail.clone(), theme.item_desc)]
            };
            lines.extend(
                entry
                    .description
                    .as_deref()
                    .unwrap_or(if entry.read_only_policy {
                        READ_ONLY_POLICY
                    } else if entry.project_config_action.is_some() {
                        "Enter reviews the trust change before confirmation."
                    } else {
                        INACTIVE_ALLOW
                    })
                    .lines()
                    .map(|line| Line::from(line.to_owned())),
            );
            lines
        } else {
            vec![Line::from(EMPTY)]
        };
        let body = Rect {
            width: inner.width.saturating_sub(SCROLLBAR_WIDTH),
            ..inner
        };
        let paragraph = Paragraph::new(lines)
            .style(theme.item)
            .wrap(Wrap { trim: false });
        let total = u16::try_from(paragraph.line_count(body.width.max(1))).unwrap_or(u16::MAX);
        self.detail.scroll.update_dimensions(total, body.height);
        let offset = self.detail.scroll.offset();
        frame.render_widget(paragraph.scroll((offset, 0)), body);
        self.detail.scrollbar.draw(frame, inner, total, offset);
        self.detail.popup = area;
    }

    fn discovery_lines(&self) -> Vec<Line<'static>> {
        let theme = theme::current();
        let mut lines = vec![Line::styled(
            format!("Discovery: {}", self.discovery.label()),
            theme.panel_title,
        )];
        let min_sessions = match &self.discovery {
            DiscoveryState::Complete(outcome) => match outcome.as_ref() {
                PatternDiscoveryOutcome::Ready(report) => report.recognizer_limits.min_sessions,
                _ => RecognizerLimits::default().min_sessions,
            },
            _ => RecognizerLimits::default().min_sessions,
        };
        let threshold = format!(
            "Patterns need support from at least {min_sessions} independent sessions; repeated commands in one session are not enough."
        );
        let empty = !self.entries.iter().any(|entry| entry.suggestion.is_some());
        let empty_sample = empty
            && matches!(&self.discovery, DiscoveryState::Complete(outcome) if matches!(outcome.as_ref(), PatternDiscoveryOutcome::Ready(_)));
        if empty_sample {
            lines.push(Line::from(threshold.clone()));
        }
        match &self.discovery {
            DiscoveryState::Idle => lines.push(Line::from(DISCOVERY_IDLE)),
            DiscoveryState::Loading => lines.push(Line::from(DISCOVERY_LOADING)),
            DiscoveryState::Cancelled => lines.push(Line::from(DISCOVERY_CANCELLED)),
            DiscoveryState::Complete(outcome) => match outcome.as_ref() {
                PatternDiscoveryOutcome::Unavailable(reason) => {
                    lines.push(Line::styled(*reason, theme.error))
                }
                PatternDiscoveryOutcome::Ready(report) => {
                    for (label, count, limit) in [
                        (
                            "Sessions sampled",
                            report.sample.sessions,
                            report.history_limits.max_sessions,
                        ),
                        (
                            "History rows",
                            report.sample.rows,
                            report.history_limits.max_rows,
                        ),
                        (
                            "History bytes",
                            report.sample.bytes,
                            report.history_limits.max_bytes,
                        ),
                        ("Tool calls", report.calls, report.max_calls),
                        (
                            "Analysis bytes",
                            report.analysis_bytes,
                            report.max_analysis_bytes,
                        ),
                        (
                            "Observations retained",
                            report.recognition.retained_observations,
                            report.recognizer_limits.max_observations,
                        ),
                    ] {
                        lines.push(Line::from(vec![
                            Span::styled(format!("{label}: "), theme.item_desc),
                            Span::raw(format!("{count} / {limit}")),
                        ]));
                    }
                    lines.push(Line::from(format!(
                        "Time budget: {} ms · row limit: {} bytes",
                        report.max_elapsed_ms, report.history_limits.max_row_bytes
                    )));
                    lines.push(Line::from(format!(
                        "{} / {} proposals found · {} visible",
                        report.candidates.len(),
                        report.recognizer_limits.max_suggestions,
                        self.entries
                            .iter()
                            .filter(|entry| entry.suggestion.is_some())
                            .count()
                    )));
                    for reason in &report.partial_reasons {
                        lines.push(Line::styled(format!("Partial: {reason}"), theme.error));
                    }
                }
            },
        }
        if !empty_sample {
            lines.push(Line::from(threshold));
        }
        if empty {
            lines.push(Line::from(DISCOVERY_EMPTY));
        }
        lines.push(Line::styled(DISCOVERY_WARNING, theme.item_desc));
        lines.push(Line::styled(SUGGESTED_GUIDANCE, theme.panel_title));
        lines
    }

    fn discovery_action(&mut self, cancel: bool) -> PermissionsPickerAction {
        self.show_discovery_overview();
        match (&self.discovery, cancel) {
            (DiscoveryState::Loading, true) => PermissionsPickerAction::CancelDiscovery,
            (DiscoveryState::Loading, false) | (_, true) => PermissionsPickerAction::Consumed,
            (_, false) => PermissionsPickerAction::RefreshDiscovery,
        }
    }

    fn selection_changed(&mut self, previous: Option<usize>) {
        if previous != self.picker.selected_index() {
            self.toolbar_hits.clear();
            self.scope_view = ScopeView::default();
            self.detail.scroll.reset();
            self.discovery_view = false;
            self.notice = None;
        }
    }

    fn map_action(&mut self, action: PickerAction<PermissionEntry>) -> PermissionsPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => PermissionsPickerAction::Consumed,
            PickerAction::Select(entry) => {
                self.detail_focused = true;
                self.discovery_view = false;
                let search = self.picker.search_text();
                self.picker.open(self.visible_entries(), self.mode.title());
                self.picker.set_search_text(&search);
                self.picker.select_item_by(|candidate| candidate == &entry);
                if let Some(action) = entry.project_config_action {
                    self.confirm_project_config_action(action);
                }
                PermissionsPickerAction::Consumed
            }
            PickerAction::Close => PermissionsPickerAction::Close,
            PickerAction::Key(key) => self.handle_key(key),
        }
    }

    fn inspect_suggestion(&mut self) {
        if self
            .picker
            .selected_item()
            .is_some_and(|entry| entry.suggestion.is_some())
        {
            self.suggestion_inspector = Some(SuggestionInspector::new());
        }
    }

    fn confirm_project_config_action(&mut self, action: ProjectConfigAction) {
        self.pending_project_config_action = Some(action);
        let message = match action {
            ProjectConfigAction::Trust => {
                "Trust the exact project shell allow configuration? Edits invalidate trust. Press Enter/y to confirm or Esc to cancel."
            }
            ProjectConfigAction::RevokeTrust => {
                "Revoke trust in this project permission config? Its shell allows will become inactive. Press Enter/y to confirm or Esc to cancel."
            }
        };
        self.notice = Some(message.into());
        self.detail.scroll.reset();
    }

    fn confirm_revoke(&mut self, id: String) {
        self.pending_revoke = Some(id);
        self.notice = Some(format!(
            "{CONFIRM_REVOKE_MESSAGE}\nOpaque constraints remain unknown; removing Deny/Ask can increase authority. Already-running calls cannot be undone."
        ));
        self.detail.scroll.reset();
    }

    fn show_read_only(&mut self, read_only_policy: bool) {
        let message = if read_only_policy {
            READ_ONLY_POLICY
        } else {
            INACTIVE_ALLOW
        };
        self.notice = Some(message.into());
        self.detail.scroll.reset();
    }

    fn has_pending_confirmation(&self) -> bool {
        self.pending_revoke.is_some() || self.pending_project_config_action.is_some()
    }

    fn manage(&mut self, launch: EditorLaunch) -> PermissionsPickerAction {
        PermissionsPickerAction::Editor(EditorEvent::Begin(launch))
    }

    pub(crate) fn set_current_project(&mut self, project: Option<PathBuf>) {
        if self.current_project != project {
            self.current_project = project;
            self.scope_view = ScopeView::default();
            self.toolbar_hits.clear();
            if let Some(editor) = &mut self.editor {
                editor.suspend();
            }
        }
    }

    fn cycle_project_filter(&mut self) {
        if self.mode != PermissionsMode::Rules || self.has_pending_confirmation() {
            return;
        }
        self.project_filter = match self.project_filter {
            ProjectFilter::All => ProjectFilter::Here,
            ProjectFilter::Here => ProjectFilter::Other,
            ProjectFilter::Other => ProjectFilter::History,
            ProjectFilter::History => ProjectFilter::All,
        };
        self.picker.replace_items(self.visible_entries());
        self.scope_view = ScopeView::default();
        self.tabs_hits.clear();
        self.toolbar_hits.clear();
    }

    fn edit_selected(&mut self, duplicate: bool) -> PermissionsPickerAction {
        match self
            .picker
            .selected_item()
            .map(|entry| entry.source.clone())
        {
            Some(PickerEntry::Stored(record)) => self.manage(if duplicate {
                EditorLaunch::Duplicate(record)
            } else {
                EditorLaunch::Edit(record)
            }),
            Some(PickerEntry::Discovered(candidate)) => {
                self.manage(EditorLaunch::Discover(candidate))
            }
            Some(PickerEntry::Policy {
                locator: Some(locator),
                ..
            }) if !duplicate => PermissionsPickerAction::EditSource(locator),
            Some(_) => {
                self.show_read_only(true);
                self.detail_focused = true;
                PermissionsPickerAction::Consumed
            }
            None => PermissionsPickerAction::Consumed,
        }
    }

    pub(crate) fn set_editor(&mut self, editor: ScopeEditor) {
        self.editor = Some(editor);
    }

    fn copy_selected(&mut self) -> PermissionsPickerAction {
        if let Some(PickerEntry::Stored(source)) =
            self.picker.selected_item().map(|entry| &entry.source)
        {
            self.manage(EditorLaunch::Copy {
                source: source.clone(),
                draft: None,
            })
        } else {
            self.show_read_only(true);
            PermissionsPickerAction::Consumed
        }
    }

    pub(crate) fn editor_mut(&mut self) -> Option<&mut ScopeEditor> {
        self.editor.as_mut()
    }

    pub(crate) fn close_editor(&mut self) {
        self.editor = None;
    }
}

impl Overlay for PermissionsPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.pending_revoke = None;
        self.pending_project_config_action = None;
        self.suggestion_inspector = None;
        self.toolbar_hits = FooterHits::default();
        self.tabs_hits = FooterHits::default();
        self.picker.close();
    }
}

fn project_config_entry(action: ProjectConfigAction) -> PermissionEntry {
    let trusted = action == ProjectConfigAction::RevokeTrust;
    PermissionEntry {
        source: PickerEntry::ProjectConfig,
        id: None,
        tool: "Project permissions.toml".into(),
        detail: if trusted {
            "shell allow patterns are active · trust can be revoked"
        } else {
            "shell allow patterns are inactive · no authority has been granted"
        }
        .into(),
        description: None,
        read_only_policy: false,
        project_config_action: Some(action),
        suggestion: None,
    }
}

fn suggestion_entry(
    project: &Path,
    revision: u64,
    candidate: &PatternCandidate,
) -> Option<PermissionEntry> {
    let definition = &candidate.definition;
    if Path::new(&definition.context.path_binding) != project {
        return None;
    }
    let definition_id = definition.fingerprint().ok()?;
    let detail = format!(
        "[{}] {} observations · {} sessions · not active",
        candidate.evidence.review_origin(),
        candidate.evidence.support.observations,
        candidate.evidence.support.independent_sessions,
    );
    let template = suggestion_template(definition, None);
    let mut lines = vec![format!("Command template: {template}"), detail.clone()];
    for slot in definition.slots.iter().take(MAX_DISPLAY_ITEMS) {
        lines.push(format!(
            "{}: {}",
            suggestion_slot(definition, slot.id),
            suggestion_domain(&slot.domain)
        ));
        lines.push(
            match slot.option_like {
                OptionLikePolicy::Reject => "  Leading '-' is rejected for this argument.",
                OptionLikePolicy::AllowForProvenData => {
                    "  Leading '-' allowed only for proven data arguments."
                }
            }
            .into(),
        );
    }
    if definition.slots.len() > MAX_DISPLAY_ITEMS {
        lines.push(OMITTED.into());
    }
    lines.push(match &definition.combinations {
        SlotCombinations::ObservedTuples { tuples } => {
            format!("Combinations: {} observed tuples only", tuples.len())
        }
        SlotCombinations::Independent => {
            "Combinations: independent; new combinations allowed".into()
        }
    });
    for (label, value) in [
        ("Working directory", &definition.context.effective_workdir),
        ("Project", &definition.context.path_binding),
        ("Tool identity", &definition.context.tool_identity),
        (
            "Executable identity",
            &definition.context.executable_identity,
        ),
        ("Analysis version", &definition.context.analysis_version),
    ] {
        lines.push(format!("{label}: {}", suggestion_literal(value)));
    }
    lines.push(SUGGESTED_GUIDANCE.into());
    lines.push(candidate.evidence.review_summary());
    for example in candidate
        .evidence
        .tuples
        .iter()
        .take(MAX_SUGGESTION_EXAMPLES)
    {
        lines.push(format!(
            "Observed example ({} observations): {}",
            example.support.observations,
            suggestion_template(definition, Some(&example.values))
        ));
    }
    if candidate.evidence.tuples.len() > MAX_SUGGESTION_EXAMPLES {
        lines.push(OMITTED.into());
    }
    lines.push(format!(
        "Name (untrusted label): {}",
        suggestion_literal(&definition.name)
    ));
    for source in candidate.evidence.sources.iter().take(MAX_DISPLAY_ITEMS) {
        lines.push(format!(
            "Source (untrusted label): {}",
            suggestion_literal(source)
        ));
    }
    if candidate.evidence.sources.len() > MAX_DISPLAY_ITEMS {
        lines.push(OMITTED.into());
    }
    Some(PermissionEntry {
        source: PickerEntry::Discovered(Arc::new(candidate.clone())),
        id: None,
        tool: template,
        detail,
        description: Some(bounded_text(&lines.join("\n"), MAX_SUGGESTION_DETAIL_CHARS)),
        read_only_policy: false,
        project_config_action: None,
        suggestion: Some(SuggestedPatternTarget {
            project: project.to_path_buf(),
            revision,
            definition_id,
        }),
    })
}

fn suggestion_slot(definition: &PatternDefinition, id: SlotId) -> String {
    let index = definition
        .slots
        .iter()
        .position(|slot| slot.id == id)
        .unwrap_or_default();
    format!("<arg{}>", index + 1)
}

fn suggestion_literal(value: &str) -> String {
    shell_words::quote(&display_text(value, MAX_FIELD_CHARS)).into_owned()
}

fn suggestion_template(definition: &PatternDefinition, values: Option<&ObservedTuple>) -> String {
    let mut words: Vec<_> = definition
        .argv
        .iter()
        .take(MAX_DISPLAY_ITEMS)
        .map(|token| match token {
            PatternToken::Exact { value, .. } => suggestion_literal(value),
            PatternToken::Slot { id, .. } => values
                .and_then(|values| values.get(id))
                .map(|value| suggestion_literal(value))
                .unwrap_or_else(|| suggestion_slot(definition, *id)),
        })
        .collect();
    if definition.argv.len() > MAX_DISPLAY_ITEMS {
        words.push(OMITTED.into());
    }
    bounded_text(&words.join(" "), MAX_DISPLAY_CHARS)
}

fn suggestion_domain(domain: &ArgumentDomain) -> String {
    match domain {
        ArgumentDomain::ObservedSet { values } => {
            let mut literals: Vec<_> = values
                .iter()
                .take(MAX_DISPLAY_ITEMS)
                .map(|value| suggestion_literal(value))
                .collect();
            if values.len() > MAX_DISPLAY_ITEMS {
                literals.push(OMITTED.into());
            }
            format!(
                "Observed values ({}): {}",
                values.len(),
                literals.join(", ")
            )
        }
        ArgumentDomain::Exact { value } => format!("Exact literal: {}", suggestion_literal(value)),
        ArgumentDomain::Glob { pattern } => format!(
            "Whole-argument glob (no shell expansion): {}",
            suggestion_literal(pattern)
        ),
        ArgumentDomain::Regex { pattern } => {
            format!("Whole-argument regex: {}", suggestion_literal(pattern))
        }
        ArgumentDomain::AnyLiteralArgument => {
            "Any single literal argument; not a command fragment".into()
        }
    }
}

fn entry(record: PermissionRuleRecord) -> PermissionEntry {
    let fallback_tool = match &record.rule.subject {
        PermissionSubject::Native { contract, .. } => contract.clone(),
        PermissionSubject::Lua { plugin, tool, .. } => format!("{plugin}:{tool}"),
        PermissionSubject::Mcp { server, tool, .. } => format!("{server}.{tool}"),
        PermissionSubject::RemoteWorkcell { tool, .. } => format!("remote.{tool}"),
        PermissionSubject::RemoteNative { owner, .. } => format!("remote.{owner}"),
        PermissionSubject::UnknownLegacy { identity } => identity.clone(),
    };
    let review = record.review.as_ref();
    let tool = display_text(
        record
            .label
            .as_ref()
            .unwrap_or_else(|| review.map_or(&fallback_tool, |review| &review.tool)),
        MAX_FIELD_CHARS,
    );
    let mut description = vec![tool.clone()];
    if let Some(review) = review {
        description.push(format!(
            "Authority: {}",
            display_text(&review.authority, MAX_FIELD_CHARS)
        ));
        description.push(format!(
            "Review: {}",
            match review.source {
                PermissionReviewSource::Approved => "approved",
                PermissionReviewSource::Recovered => "recovered",
                PermissionReviewSource::Unavailable => UNAVAILABLE,
            }
        ));
    } else {
        description.push(format!("Review: {UNAVAILABLE}"));
    }
    let mut scopes = Vec::new();
    for (index, constraint) in record
        .rule
        .resources
        .iter()
        .take(MAX_DISPLAY_ITEMS)
        .enumerate()
    {
        let resource = review.and_then(|review| {
            review
                .resources
                .iter()
                .find(|resource| resource.index == index)
        });
        let value = resource
            .and_then(|resource| resource.value.as_deref())
            .unwrap_or(UNAVAILABLE);
        scopes.push(display_text(value, MAX_FIELD_CHARS));
        let kind = match &constraint.kind {
            PermissionResourceKind::File => "File",
            PermissionResourceKind::Directory => "Directory",
            PermissionResourceKind::Url => "URL",
            PermissionResourceKind::Command => "Command",
            PermissionResourceKind::Query => "Query",
            PermissionResourceKind::RemoteFile { .. } => "Remote file",
            PermissionResourceKind::RemoteDirectory { .. } => "Remote directory",
            PermissionResourceKind::RemoteResource { resource_kind, .. } => resource_kind,
            PermissionResourceKind::Custom { name } => name,
        };
        description.push(format!(
            "Scope {} ({}): {} · access {} · protected {}",
            index + 1,
            display_text(kind, MAX_FIELD_CHARS),
            display_text(value, MAX_FIELD_CHARS),
            constraint
                .access
                .as_ref()
                .map_or_else(|| "any".into(), |access| format!("{access:?}")),
            constraint
                .protected
                .map_or_else(|| "any".into(), |protected| protected.to_string())
        ));
        for key in constraint.attributes.keys().take(MAX_DISPLAY_ITEMS) {
            let value = resource
                .and_then(|resource| resource.attributes.get(key))
                .map(String::as_str)
                .unwrap_or(UNAVAILABLE);
            description.push(format!(
                "{}: {}",
                display_text(key, MAX_FIELD_CHARS),
                display_text(value, MAX_FIELD_CHARS)
            ));
        }
        if constraint.attributes.len() > MAX_DISPLAY_ITEMS {
            description.push(OMITTED.into());
        }
    }
    if record.rule.resources.len() > MAX_DISPLAY_ITEMS {
        description.push(OMITTED.into());
    }
    let input = review
        .and_then(|review| review.input.as_ref())
        .map(|input| input_description(input, 0))
        .unwrap_or_else(|| {
            if matches!(
                record.rule.arguments,
                PermissionArgumentConstraint::Unconstrained
            ) {
                "unconstrained".into()
            } else {
                UNAVAILABLE.into()
            }
        });
    description.push(format!("Input: {input}"));
    let detail = format!(
        "[{}] {} · {}",
        authority_badge(&record),
        effect_name(&record.rule.effect),
        lifetime_name(&record.rule.lifetime),
    );
    description.insert(1, detail.clone());
    let label = if scopes.is_empty() {
        input
    } else {
        scopes.join(" · ")
    };
    PermissionEntry {
        id: Some(record.id.clone()),
        source: PickerEntry::Stored(Arc::new(record)),
        tool: display_text(&format!("{tool} · {label}"), MAX_DISPLAY_CHARS),
        detail,
        description: Some(bounded_text(&description.join("\n"), MAX_DISPLAY_CHARS)),
        read_only_policy: false,
        project_config_action: None,
        suggestion: None,
    }
}

fn bounded_text(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut bounded: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        bounded.push_str(OMITTED);
    }
    bounded
}

fn display_text(text: &str, limit: usize) -> String {
    let escaped = escape_terminal_controls(&bounded_text(text, limit));
    let mut safe = String::new();
    for character in escaped.chars() {
        if matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    bounded_text(&safe, limit)
}

fn input_description(value: &Value, depth: usize) -> String {
    if depth >= MAX_INPUT_DEPTH {
        return OMITTED.into();
    }
    let (mut items, count) = match value {
        Value::Object(fields) => (
            fields
                .iter()
                .take(MAX_DISPLAY_ITEMS)
                .map(|(key, value)| {
                    format!(
                        "{}: {}",
                        display_text(key, MAX_FIELD_CHARS),
                        input_description(value, depth + 1)
                    )
                })
                .collect::<Vec<_>>(),
            fields.len(),
        ),
        Value::Array(values) => (
            values
                .iter()
                .take(MAX_DISPLAY_ITEMS)
                .map(|value| input_description(value, depth + 1))
                .collect::<Vec<_>>(),
            values.len(),
        ),
        Value::String(value) => return display_text(value, MAX_FIELD_CHARS),
        value => return value.to_string(),
    };
    if count > MAX_DISPLAY_ITEMS {
        items.push(OMITTED.into());
    }
    bounded_text(&items.join(", "), MAX_DISPLAY_CHARS)
}

fn review_entry(candidate: &PermissionReviewCandidate) -> PermissionEntry {
    let source = match candidate.source {
        PermissionSource::Global => "global config",
        PermissionSource::Project => "project config",
        PermissionSource::Conversation => "legacy conversation",
    };
    let kind = match candidate.kind {
        PermissionReviewKind::Rule => "allow rule",
        PermissionReviewKind::Default => "allow default",
    };
    let tool = candidate
        .tool
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "*".into());
    let scope = candidate.scope.as_deref().unwrap_or("<all>");
    PermissionEntry {
        source: PickerEntry::Review(candidate.clone()),
        id: None,
        tool: display_text(&tool, MAX_FIELD_CHARS),
        detail: format!(
            "[needs review] inactive {kind} · {source} · scope {}",
            display_text(scope, MAX_FIELD_CHARS)
        ),
        description: None,
        read_only_policy: false,
        project_config_action: None,
        suggestion: None,
    }
}

fn policy_entry(entry: &ActivePolicyRule) -> PermissionEntry {
    let scope = entry.rule.scope.as_deref().unwrap_or("<all>");
    PermissionEntry {
        source: PickerEntry::Policy {
            source: entry.source,
            rule: entry.rule.clone(),
            locator: entry.verified_local_source_locator.clone(),
        },
        id: None,
        tool: display_text(&entry.rule.tool.to_string(), MAX_FIELD_CHARS),
        detail: format!(
            "[policy] {} · {} · scope {} · read-only",
            match entry.rule.effect {
                Effect::Allow => "allow",
                Effect::Ask => "ask",
                Effect::Deny => "deny",
            },
            display_text(entry.source, MAX_FIELD_CHARS),
            display_text(scope, MAX_FIELD_CHARS),
        ),
        description: None,
        read_only_policy: true,
        project_config_action: None,
        suggestion: None,
    }
}

fn authority_badge(record: &PermissionRuleRecord) -> String {
    if record.rule.family.is_some()
        || record.rule.resources.iter().any(|resource| {
            matches!(
                resource.selector,
                PermissionResourceSelector::CommandTemplate { .. }
                    | PermissionResourceSelector::RemoteResource { .. }
                    | PermissionResourceSelector::RemoteSubtree { .. }
                    | PermissionResourceSelector::Prefix { .. }
                    | PermissionResourceSelector::Subtree { .. }
            )
        })
    {
        return rule_kind(&record.rule);
    }
    let argument = match record.rule.arguments {
        PermissionArgumentConstraint::Exact { .. } => "exact",
        PermissionArgumentConstraint::Selected { .. }
        | PermissionArgumentConstraint::SelectedDigest { .. } => "selected",
        PermissionArgumentConstraint::Unconstrained
            if record.rule.resources.is_empty()
                || record.rule.resources.iter().any(|resource| {
                    matches!(resource.selector, PermissionResourceSelector::Any)
                }) =>
        {
            "any"
        }
        PermissionArgumentConstraint::Unconstrained => "resource",
    };
    let selector = if record.rule.resources.iter().any(|resource| {
        matches!(
            resource.selector,
            PermissionResourceSelector::UrlOriginDigest { .. }
        )
    }) {
        "origin/**"
    } else if record.rule.resources.iter().any(|resource| {
        matches!(
            resource.selector,
            PermissionResourceSelector::CommandPattern { .. }
        )
    }) {
        "command-pattern"
    } else if record.rule.resources.iter().any(|resource| {
        matches!(
            resource.selector,
            PermissionResourceSelector::UrlSubtreeDigest { .. }
        )
    }) {
        "url/**"
    } else if record.rule.resources.iter().any(|resource| {
        matches!(
            resource.selector,
            PermissionResourceSelector::FilesystemSubtreeDigest { .. }
        )
    }) {
        "directory/**"
    } else if record
        .rule
        .resources
        .iter()
        .any(|resource| matches!(resource.selector, PermissionResourceSelector::Any))
    {
        "*"
    } else {
        "exact-resource"
    };
    format!("{argument}:{selector}")
}

fn effect_name(effect: &StructuredPermissionEffect) -> &'static str {
    match effect {
        StructuredPermissionEffect::Allow => "allow",
        StructuredPermissionEffect::Deny => "deny",
        StructuredPermissionEffect::Ask => "ask",
    }
}

fn lifetime_name(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => "once",
        PermissionLifetime::Conversation => "conversation",
        PermissionLifetime::Project => "project",
        PermissionLifetime::Global => "global",
    }
}

fn footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "Inspect"),
        Hint::bind(key::TAB, "List/Detail"),
        Hint::bind(key::ESC, "Close"),
    ]
}

fn trust_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "Trust or revoke"),
        Hint::bind(key::TAB, "Details"),
        Hint::bind(key::ESC, "Close"),
    ]
}

fn suggestion_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "View"),
        Hint::bind(DISMISS_SUGGESTION, "Dismiss"),
        Hint::bind(SNOOZE_SUGGESTION, "Snooze"),
    ]
}

fn discovery_footer() -> Vec<Hint> {
    vec![
        Hint::bind(REFRESH_DISCOVERY, "Refresh"),
        Hint::bind(key::TAB, "Details"),
        Hint::bind(key::ESC, "Back"),
    ]
}

/// Drawn inside the inspector's wrapped, scrolling paragraph, where no row
/// is fixed enough to hold a hit rect, so it is text alone.
fn suggestion_inspector_footer() -> Line<'static> {
    hint_line(&[
        Hint::inert("PgUp/PgDn", "Scroll"),
        Hint::bind(DISMISS_SUGGESTION, "Dismiss"),
        Hint::bind(SNOOZE_SUGGESTION, "Snooze 24h"),
        Hint::bind(key::ESC, "Back"),
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::fs::{self, Permissions};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;
    use std::sync::Arc;

    use caudra_agent::permissions::{
        PermissionArgumentConstraint, PermissionLifetime, PermissionRequest,
        PermissionResourceKind, PermissionResourceSelector, PermissionReviewSource,
        PermissionRuleRecord, StructuredPermissionEffect,
        pattern_recognition::{
            CandidateEvidence, InvocationOutcome, ObservationProvenance, PatternCandidate,
            SupportCount, TupleSupport,
        },
        review::{review_for_rule, review_from_candidates},
        selected_input_digest,
    };
    use caudra_config::{
        PermissionReviewCandidate, PermissionReviewKind, PermissionSource, ToolKey,
    };
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, OptionLikePolicy, PATTERN_SCHEMA_VERSION, PatternContext,
        PatternDefinition, PatternSlot, PatternToken, SlotCombinations, SlotId,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, CellWidth};
    use ratatui::layout::Position;
    use serde_json::{Value, json};
    use tempfile::Builder;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    use crate::components::permission_scope::view::Disclosure;
    use crate::components::{buffer_text, list_picker::PickerItem};
    use crate::{PatternDiscoveryOutcome, test_pattern_discovery_report, theme};

    use super::{
        DISCOVERY_CANCELLED, DISCOVERY_EMPTY, DISCOVERY_IDLE, DISCOVERY_LOADING, DISCOVERY_WARNING,
        DiscoveryState, MAX_DISPLAY_CHARS, MAX_FIELD_CHARS, MAX_SUGGESTION_DETAIL_CHARS, OMITTED,
        PermissionsMode, PermissionsPicker, PermissionsPickerAction, PickerAction,
        ProjectConfigAction, SCROLLBAR_WIDTH, SUGGESTED_GUIDANCE, SUGGESTED_SECTION, UNAVAILABLE,
        display_text, entry, project_config_entry, suggestion_entry,
    };

    const ROOT: &str = "/project";
    const FILE: &str = "/project/src/main.rs";
    const PATTERN: &str = "**/*.{rs,toml}";
    const COMMAND: &str = "git status --short";
    const COMMAND_PATTERN: &str = "git status *";
    const SECRET: &str = "top-secret";
    const REDACTED: &str = "[redacted]";
    const CONFIRM_REVOKE: &str = "Revoke this permission?";
    const SUGGESTED_COMMAND: &str = "cargo test <arg1>";
    const SUGGESTED_OBSERVATIONS: &str = "4 observations";
    const SUGGESTED_SESSIONS: &str = "2 sessions";
    const SUGGESTED_VALUES: [&str; 2] = ["alpha", "beta"];
    const SUGGESTION_REVISION: u64 = 7;
    const NATIVE_ORIGIN: &str = "Native observations";
    const IMPORTED_ORIGIN: &str = "Imported history (unverified)";
    const REQUESTED_OUTCOMES: &str = "4 requested (execution outcome not recorded)";
    const UNKNOWN_OUTCOMES: &str = "4 unknown outcomes";
    const UNRECORDED_OUTCOMES: &str = "Outcomes: not recorded.";
    const HISTORICAL_CONTEXT: &str =
        "Historical execution context and tool identity are unverified";
    const HISTORICAL_ASSUMPTIONS: &str = "Analysis assumes standard Bash startup";
    const HISTORICAL_PROJECT: &str = "current stored cwd approximates its historical project";
    const NARROW_WIDTH: u16 = 40;
    const SHORT_HEIGHT: u16 = 12;
    const LONG_WORD_REPEATS: usize = 24;
    const NAV_LABELS: [&str; 3] = ["short-rule", "long-rule", "last-rule"];
    const LONG_DETAIL: &str = "long constrained argument value ";
    const LONG_DETAIL_REPEATS: usize = 80;
    const MANAGER_TEST_HEIGHT: u16 = 40;
    const DISCOVERY_TEST_ERROR: &str = "History is locked. Refresh to retry.";
    const DISCOVERY_TEST_PARTIAL: &str = "History sample limit reached";
    const DISCOVERY_SAMPLE_COUNT: &str = "Sessions sampled: 2 / 64";
    const DISCOVERY_MINIMUM: &str = "at least 2 independent sessions";
    const EXPORT_DIRECTORY_MODE: u32 = 0o700;
    const EXPORT_ARTIFACT_MODE: u32 = 0o600;
    const EXPORT_LONG_GRANTS: usize = 240;
    const EXPORT_MAX_PAGES: usize = 32;
    const EXPORT_SCAN_BUDGET_MS: u64 = 3_000;
    const EXPORT_ROW_BYTES: usize = 512;
    const EXPORT_LONG_COMMAND: &str = "opsctl release inspect --project warehouse --environment staging --format json --include dependencies --include rollout --include health --target 'deploy/東京/warehouse-green' --config '/project/deployments/production/release coordination/rollout and recovery settings.toml'";
    const EXPORT_UNICODE_PATH: &str =
        "/project/docs/設計/rollout notes/production/recovery checklist.md";
    const EXPORT_ANCHORS: [&str; 2] = ["shell · Exact: git status", "shell · Exact: opsctl"];
    const EXPORT_LONG_ANCHORS: [&str; 2] = [
        "shell · Exact: opsctl task inspect --id task-000",
        "shell · Exact: opsctl task inspect --id task-001",
    ];
    const MODE_TEST_RULE_INDEX: usize = 17;
    const PROPOSAL_ANCHORS: [&str; 2] = ["opsctl release inspect", "artifactctl artifact describe"];

    fn suggestion() -> PatternCandidate {
        let slot = SlotId(1);
        let tuples = SUGGESTED_VALUES
            .iter()
            .map(|value| [(slot, (*value).into())].into())
            .collect();
        PatternCandidate {
            definition: PatternDefinition {
                version: PATTERN_SCHEMA_VERSION,
                name: "Cargo tests".into(),
                context: PatternContext {
                    tool_identity: "workcell:shell".into(),
                    executable_identity: "cargo".into(),
                    effective_workdir: ROOT.into(),
                    path_binding: ROOT.into(),
                    analysis_version: "test-v1".into(),
                },
                argv: vec![
                    PatternToken::Exact {
                        value: "cargo".into(),
                        role: ArgumentRole::Executable,
                    },
                    PatternToken::Exact {
                        value: "test".into(),
                        role: ArgumentRole::Operation,
                    },
                    PatternToken::Slot {
                        id: slot,
                        role: ArgumentRole::Data,
                    },
                ],
                slots: vec![PatternSlot {
                    id: slot,
                    label: "package".into(),
                    domain: ArgumentDomain::ObservedSet {
                        values: SUGGESTED_VALUES
                            .iter()
                            .map(|value| (*value).into())
                            .collect(),
                    },
                    option_like: OptionLikePolicy::Reject,
                }],
                combinations: SlotCombinations::ObservedTuples { tuples },
            },
            evidence: CandidateEvidence {
                support: SupportCount {
                    observations: 4,
                    independent_sessions: 2,
                },
                provenance: ObservationProvenance::Native,
                sources: ["runtime".into()].into(),
                outcomes: [(InvocationOutcome::Requested, 4)].into(),
                first_seen_ms: 0,
                last_seen_ms: 0,
                distributions: Default::default(),
                tuples: SUGGESTED_VALUES
                    .iter()
                    .map(|value| TupleSupport {
                        values: [(slot, (*value).into())].into(),
                        support: SupportCount {
                            observations: 2,
                            independent_sessions: 1,
                        },
                    })
                    .collect(),
            },
        }
    }

    fn suggested_picker() -> PermissionsPicker {
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record()], &[], &[], false, false);
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION, &[suggestion()]);
        picker.show_discovery();
        assert!(
            picker
                .picker
                .select_item_by(|entry| entry.suggestion.is_some())
        );
        picker
    }

    fn compact(text: &str) -> String {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn read_suggestion(picker: &mut PermissionsPicker, width: u16, height: u16) -> String {
        if picker.suggestion_inspector.is_none() {
            picker.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut rows = BTreeMap::new();
        loop {
            terminal
                .draw(|frame| {
                    picker.view(frame, frame.area());
                })
                .unwrap();
            let inspector = picker.suggestion_inspector.as_ref().unwrap();
            let offset = inspector.scroll.offset();
            let popup = inspector.popup;
            for (index, y) in (popup.y + 1..popup.bottom() - 1).enumerate() {
                let mut row = String::new();
                let mut x = popup.x + 1;
                let end = popup.right() - 1 - SCROLLBAR_WIDTH;
                while x < end {
                    let cell = &terminal.backend().buffer()[(x, y)];
                    row.push_str(cell.symbol());
                    x += cell.cell_width().max(1);
                    assert!(x <= end, "grapheme extends past the inspector body");
                }
                rows.insert(usize::from(offset) + index, row);
            }
            picker.handle_key(KeyEvent::from(KeyCode::Down));
            if picker
                .suggestion_inspector
                .as_ref()
                .unwrap()
                .scroll
                .offset()
                == offset
            {
                break;
            }
        }
        compact(&rows.into_values().collect::<String>())
    }

    fn read_details(picker: &mut PermissionsPicker, width: u16, height: u16) -> String {
        picker.detail_focused = true;
        picker.detail.scroll.reset();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        if picker
            .picker
            .selected_item()
            .and_then(super::PermissionEntry::scope)
            .is_some()
            && !picker.discovery_view
            && picker.notice.is_none()
        {
            let mut text = String::new();
            for disclosure in [None, Some(Disclosure::Evidence)] {
                picker.scope_view.disclosure = disclosure;
                picker.scope_view.offset = 0;
                let mut previous = None;
                for _ in 0..MAX_DISPLAY_CHARS {
                    terminal
                        .draw(|frame| {
                            picker.view(frame, frame.area());
                        })
                        .unwrap();
                    if previous == Some(picker.scope_view.offset) {
                        break;
                    }
                    previous = Some(picker.scope_view.offset);
                    text.push_str(&buffer_text(terminal.backend().buffer()));
                    picker.scope_view.scroll(1);
                }
            }
            picker.scope_view = super::ScopeView::default();
            picker.detail_focused = false;
            return compact(&text);
        }
        let mut rows = BTreeMap::new();
        loop {
            terminal
                .draw(|frame| {
                    picker.view(frame, frame.area());
                })
                .unwrap();
            let pane = picker.detail.popup;
            let offset = picker.detail.scroll.offset();
            for (index, y) in (pane.y + 1..pane.bottom().saturating_sub(1)).enumerate() {
                let mut row = String::new();
                let mut x = pane.x + 1;
                while x < pane.right().saturating_sub(1 + SCROLLBAR_WIDTH) {
                    let cell = &terminal.backend().buffer()[(x, y)];
                    row.push_str(cell.symbol());
                    x += cell.cell_width().max(1);
                }
                rows.insert(usize::from(offset) + index, row);
            }
            picker.detail.scroll.scroll(-1);
            if picker.detail.scroll.offset() == offset {
                break;
            }
        }
        picker.detail_focused = false;
        compact(&rows.into_values().collect::<String>())
    }

    fn export_rule(tool: &str, input: Value, resources: &[&str]) -> PermissionRuleRecord {
        let mut record = reviewed_record(tool, input, resources);
        record.rule.effect = StructuredPermissionEffect::Allow;
        PermissionRuleRecord::conversation_with_review(record.rule, record.review).unwrap()
    }

    fn export_proposals() -> Vec<PatternCandidate> {
        [
            ("opsctl", "release", "inspect"),
            ("artifactctl", "artifact", "describe"),
        ]
        .into_iter()
        .map(|(executable, noun, verb)| {
            let mut candidate = suggestion();
            candidate.definition.name = format!("{executable} observed targets");
            candidate.definition.context.executable_identity = executable.into();
            candidate.definition.argv = vec![
                PatternToken::Exact {
                    value: executable.into(),
                    role: ArgumentRole::Executable,
                },
                PatternToken::Exact {
                    value: noun.into(),
                    role: ArgumentRole::Unknown,
                },
                PatternToken::Exact {
                    value: verb.into(),
                    role: ArgumentRole::Unknown,
                },
                PatternToken::Exact {
                    value: "--target".into(),
                    role: ArgumentRole::Flag,
                },
                PatternToken::Slot {
                    id: SlotId(1),
                    role: ArgumentRole::Unknown,
                },
            ];
            candidate.definition.slots[0].label = "observed target".into();
            candidate.evidence.provenance = ObservationProvenance::Imported;
            candidate.evidence.sources = ["saved-history".into()].into();
            candidate.evidence.outcomes = [(InvocationOutcome::Unknown, 4)].into();
            candidate.definition.validate().unwrap();
            candidate
        })
        .collect()
    }

    fn export_picker(panel: &str) -> PermissionsPicker {
        let rules = if panel.starts_with("discovery-long-list") {
            (0..EXPORT_LONG_GRANTS)
                .map(|index| {
                    let command = format!("opsctl task inspect --id task-{index:03}");
                    export_rule(
                        "shell",
                        json!({"command": command, "workdir": ROOT}),
                        &[&command],
                    )
                })
                .collect()
        } else if panel == "discovery-empty" {
            Vec::new()
        } else {
            vec![
                export_rule(
                    "shell",
                    json!({"command": COMMAND, "workdir": ROOT}),
                    &[COMMAND],
                ),
                export_rule(
                    "shell",
                    json!({"command": EXPORT_LONG_COMMAND, "workdir": ROOT}),
                    &[EXPORT_LONG_COMMAND],
                ),
                export_rule(
                    "file_read",
                    json!({"filePath": EXPORT_UNICODE_PATH, "offset": 80, "limit": 160}),
                    &[EXPORT_UNICODE_PATH],
                ),
            ]
        };
        let mut picker = PermissionsPicker::new();
        picker.open(rules, &[], &[], false, false);
        if panel.starts_with("active-") {
            return picker;
        }
        let state = match panel {
            "discovery-loading" => DiscoveryState::Loading,
            "discovery-unavailable" => DiscoveryState::Complete(Arc::new(
                PatternDiscoveryOutcome::Unavailable(DISCOVERY_TEST_ERROR),
            )),
            _ => {
                let empty = panel == "discovery-empty";
                let candidates = if empty {
                    Vec::new()
                } else {
                    export_proposals()
                };
                picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION, &candidates);
                let mut report = test_pattern_discovery_report(candidates);
                report.history_limits.max_row_bytes *= 1024;
                report.history_limits.max_bytes =
                    report.history_limits.max_rows * report.history_limits.max_row_bytes;
                report.max_analysis_bytes = report.history_limits.max_bytes;
                report.max_elapsed_ms = EXPORT_SCAN_BUDGET_MS;
                report.sample.sessions = if empty { 1 } else { 3 };
                report.sample.rows = if empty { 16 } else { 48 };
                report.sample.bytes = report.sample.rows * EXPORT_ROW_BYTES;
                report.calls = if empty { 2 } else { 12 };
                report.analysis_bytes = report.calls * EXPORT_ROW_BYTES;
                report.recognition.retained_observations = if empty { 2 } else { 8 };
                if !empty {
                    report.sample.truncated = true;
                    report.partial_reasons.push("Per-session history row limit: 2 sessions cut short at 16 rows (main + subagents)".into());
                }
                DiscoveryState::Complete(Arc::new(PatternDiscoveryOutcome::Ready(report)))
            }
        };
        picker.set_discovery(state);
        picker.show_discovery();
        picker
    }

    fn export_buffer(picker: &mut PermissionsPicker, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn export_rows(buffer: &Buffer) -> String {
        let mut text = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            let mut x = buffer.area.x;
            while x < buffer.area.right() {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x += (symbol.width() as u16).max(1);
            }
            text.push('\n');
        }
        text
    }

    fn export_list_anchors(
        picker: &PermissionsPicker,
        text: &str,
        labels: &[&str],
    ) -> Vec<Position> {
        labels
            .iter()
            .copied()
            .map(|label| {
                text.lines()
                    .enumerate()
                    .find_map(|(y, row)| {
                        let index = row.find(label)?;
                        let position = Position::new(row[..index].width() as u16, y as u16);
                        picker.picker.contains(position).then_some(position)
                    })
                    .unwrap_or_else(|| panic!("missing list anchor {label}:\n{text}"))
            })
            .collect()
    }

    fn write_export_buffer(directory: &Path, stem: &str, buffer: &Buffer) {
        for (extension, contents) in [
            ("txt", export_rows(buffer)),
            ("cells", format!("{buffer:#?}")),
        ] {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(EXPORT_ARTIFACT_MODE);
            options
                .open(directory.join(format!("{stem}.{extension}")))
                .unwrap()
                .write_all(contents.as_bytes())
                .unwrap();
        }
    }

    #[test]
    #[ignore = "writes private visual review buffers under /tmp; run alone with --test-threads=1"]
    fn export_permissions_manager_buffers() {
        let directory = Builder::new()
            .prefix("caudra-permissions-manager-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            directory.path(),
            Permissions::from_mode(EXPORT_DIRECTORY_MODE),
        )
        .unwrap();
        for name in ["ayu_dark", "ayu_light"] {
            theme::set(theme::load_by_name(name).unwrap());
            for (width, height) in [(40, 24), (80, 24), (140, 32)] {
                let mut anchors = Vec::new();
                for panel in [
                    "active-short",
                    "active-long",
                    "discovery-loading",
                    "discovery-partial",
                    "discovery-overview",
                    "discovery-proposal",
                    "discovery-empty",
                    "discovery-unavailable",
                    "discovery-long-list",
                    "discovery-long-list-after-tab",
                    "discovery-long-list-rules",
                ] {
                    let mut picker = export_picker(panel);
                    export_buffer(&mut picker, width, height);
                    match panel {
                        "active-long" => {
                            picker.handle_key(KeyEvent::from(KeyCode::Down));
                        }
                        "discovery-proposal" => {
                            picker.handle_key(KeyEvent::from(KeyCode::Enter));
                        }
                        "discovery-overview" => {
                            picker.handle_key(KeyEvent::new(
                                KeyCode::Char('o'),
                                KeyModifiers::CONTROL,
                            ));
                        }
                        "discovery-long-list-after-tab" => {
                            picker.handle_key(KeyEvent::from(KeyCode::Tab));
                        }
                        "discovery-long-list-rules" => {
                            picker.handle_key(KeyEvent::new(
                                KeyCode::Char('g'),
                                KeyModifiers::CONTROL,
                            ));
                        }
                        _ => {}
                    }
                    let buffer = export_buffer(&mut picker, width, height);
                    let stem = format!("{panel}-{name}-{width}x{height}");
                    if panel == "active-short" {
                        anchors =
                            export_list_anchors(&picker, &export_rows(&buffer), &EXPORT_ANCHORS);
                    } else if panel == "active-long" {
                        assert_eq!(
                            export_list_anchors(&picker, &export_rows(&buffer), &EXPORT_ANCHORS),
                            anchors
                        );
                        println!("{stem}: stable list anchors {anchors:?}");
                    }
                    write_export_buffer(directory.path(), &stem, &buffer);
                    if matches!(panel, "discovery-overview" | "discovery-empty") {
                        if !picker.detail_focused {
                            picker.handle_key(KeyEvent::from(KeyCode::Tab));
                        }
                        for page in 1..EXPORT_MAX_PAGES {
                            let offset = picker.detail.scroll.offset();
                            picker.handle_key(KeyEvent::from(KeyCode::PageDown));
                            let buffer = export_buffer(&mut picker, width, height);
                            if picker.detail.scroll.offset() == offset {
                                break;
                            }
                            write_export_buffer(
                                directory.path(),
                                &format!("{stem}-details-{page:02}"),
                                &buffer,
                            );
                            assert!(page + 1 < EXPORT_MAX_PAGES);
                        }
                    }
                }
            }
        }
        println!(
            "Permissions manager buffers: {}",
            directory.keep().display()
        );
    }

    fn proposal_anchors(picker: &PermissionsPicker, text: &str) -> Vec<Position> {
        PROPOSAL_ANCHORS
            .into_iter()
            .map(|label| {
                text.lines()
                    .enumerate()
                    .find_map(|(y, row)| {
                        let index = row.find(label)?;
                        let position = Position::new(row[..index].width() as u16, y as u16);
                        picker.picker.contains(position).then_some(position)
                    })
                    .unwrap_or_else(|| panic!("missing proposal row {label}:\n{text}"))
            })
            .collect()
    }

    #[test_case(40, 24; "narrow")]
    #[test_case(80, 24; "normal")]
    #[test_case(140, 32; "wide")]
    fn discover_shows_only_proposals_above_240_rules_with_stable_pane_focus(
        width: u16,
        height: u16,
    ) {
        let mut picker = export_picker("discovery-long-list");
        assert_eq!(
            picker.entries.len(),
            EXPORT_LONG_GRANTS + PROPOSAL_ANCHORS.len()
        );
        assert_eq!(picker.visible_entries().len(), PROPOSAL_ANCHORS.len());
        assert!(picker.discovery_mode());
        assert!(!picker.detail_focused);
        assert!(!picker.discovery_view);
        let before = export_buffer(&mut picker, width, height);
        let anchors = proposal_anchors(&picker, &export_rows(&before));
        let panes = (picker.popup, picker.detail.popup);
        let tabs = (picker.tabs_hits.hit(0), picker.tabs_hits.hit(1));
        let first = picker.picker.selected_item().unwrap().suggestion.clone();
        for key in [KeyCode::Down, KeyCode::Tab, KeyCode::Tab, KeyCode::Up] {
            picker.handle_key(KeyEvent::from(key));
            let buffer = export_buffer(&mut picker, width, height);
            if !picker.detail_focused || width >= super::SIDE_BY_SIDE_WIDTH {
                assert_eq!(proposal_anchors(&picker, &export_rows(&buffer)), anchors);
                assert_eq!((picker.popup, picker.detail.popup), panes);
            }
            assert_eq!((picker.tabs_hits.hit(0), picker.tabs_hits.hit(1)), tabs);
            assert!(picker.discovery_mode());
        }
        assert!(picker.picker.selected_item().unwrap().suggestion == first);
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(picker.detail_focused);
        assert!(!picker.has_pending_confirmation());
    }

    #[test_case(40, 24, false; "keyboard_narrow")]
    #[test_case(80, 24, false; "keyboard_normal")]
    #[test_case(140, 32, false; "keyboard_wide")]
    #[test_case(40, 24, true; "mouse_narrow")]
    #[test_case(80, 24, true; "mouse_normal")]
    #[test_case(140, 32, true; "mouse_wide")]
    fn mode_tabs_restore_each_list_selection_and_focus_the_list(
        width: u16,
        height: u16,
        mouse: bool,
    ) {
        let mut picker = export_picker("discovery-long-list");
        picker.set_mode(PermissionsMode::Rules);
        picker.picker.select(MODE_TEST_RULE_INDEX);
        let rule = picker.picker.selected_item().unwrap().id.clone();
        let mut proposal = None;
        for index in [1, 0, 1] {
            export_buffer(&mut picker, width, height);
            if mouse {
                let tab = picker.tabs_hits.hit(index);
                assert!(tab.width > 0);
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    assert!(matches!(
                        picker.handle_mouse(MouseEvent {
                            kind,
                            column: tab.x,
                            row: tab.y,
                            modifiers: KeyModifiers::NONE
                        }),
                        PermissionsPickerAction::Consumed
                    ));
                }
            } else {
                picker.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
            }
            assert_eq!(picker.discovery_mode(), index == 1);
            assert!(!picker.detail_focused);
            assert!(!picker.has_pending_confirmation());
            if index == 0 {
                assert_eq!(picker.picker.selected_item().unwrap().id, rule);
                assert_eq!(picker.visible_entries().len(), EXPORT_LONG_GRANTS);
            } else if let Some(target) = &proposal {
                assert!(picker.picker.selected_item().unwrap().suggestion.as_ref() == Some(target));
            } else {
                picker.handle_key(KeyEvent::from(KeyCode::Down));
                proposal = picker.picker.selected_item().unwrap().suggestion.clone();
            }
            picker.handle_key(KeyEvent::from(KeyCode::Tab));
            assert!(picker.detail_focused);
        }
    }

    #[test_case(false; "reading_rule_details")]
    #[test_case(true; "pending_revoke")]
    fn scan_completion_never_switches_modes_or_replaces_a_rule_decision(pending: bool) {
        let mut picker = export_picker("discovery-long-list");
        picker.set_mode(PermissionsMode::Rules);
        picker.picker.select(MODE_TEST_RULE_INDEX);
        let selected = picker.picker.selected_item().unwrap().clone();
        if pending {
            picker.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        } else {
            picker.handle_key(KeyEvent::from(KeyCode::Tab));
        }
        let focused = picker.detail_focused;
        let notice = picker.notice.clone();
        let pending_revoke = picker.pending_revoke.clone();
        picker.set_discovery(DiscoveryState::Loading);
        let mut candidates = export_proposals();
        candidates.reverse();
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION + 1, &candidates);
        picker.set_discovery(DiscoveryState::Complete(Arc::new(
            PatternDiscoveryOutcome::Ready(test_pattern_discovery_report(candidates)),
        )));
        assert!(!picker.discovery_mode());
        assert!(picker.picker.selected_item() == Some(&selected));
        assert_eq!(picker.detail_focused, focused);
        assert_eq!(picker.notice, notice);
        assert_eq!(picker.pending_revoke, pending_revoke);
        if pending {
            picker.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
            assert!(!picker.discovery_mode());
            assert!(
                matches!(picker.handle_key(KeyEvent::from(KeyCode::Enter)), PermissionsPickerAction::Revoke(id) if Some(id.as_str()) == selected.id.as_deref())
            );
        }
    }

    fn navigation_picker() -> PermissionsPicker {
        let mut picker = PermissionsPicker::new();
        picker.open(Vec::new(), &[], &[], false, false);
        picker.entries = NAV_LABELS
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let mut permission = entry(record());
                permission.id = Some((*label).into());
                permission.tool = (*label).into();
                permission.description = Some(if index == 1 {
                    LONG_DETAIL.repeat(LONG_DETAIL_REPEATS)
                } else {
                    (*label).into()
                });
                permission
            })
            .collect();
        picker.picker.replace_items(picker.entries.clone());
        picker.discovery_view = false;
        picker
    }

    fn nav_positions(
        picker: &PermissionsPicker,
        terminal: &Terminal<TestBackend>,
    ) -> Vec<Position> {
        let buffer = terminal.backend().buffer();
        NAV_LABELS
            .iter()
            .map(|label| {
                (0..buffer.area.height)
                    .find_map(|y| {
                        let row: String = (0..buffer.area.width)
                            .map(|x| {
                                if picker.picker.contains(Position::new(x, y)) {
                                    buffer[(x, y)].symbol()
                                } else {
                                    " "
                                }
                            })
                            .collect();
                        row.find(*label).map(|x| Position::new(x as u16, y))
                    })
                    .unwrap()
            })
            .collect()
    }

    #[test_case(40; "narrow_stacked")]
    #[test_case(80; "stacked")]
    #[test_case(140; "side_by_side")]
    fn selected_details_never_move_navigation_or_mouse_targets(width: u16) {
        let mut picker = navigation_picker();
        let mut terminal = Terminal::new(TestBackend::new(width, MANAGER_TEST_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let rows = nav_positions(&picker, &terminal);
        let panes = (picker.popup, picker.detail.popup);
        for _ in NAV_LABELS {
            picker.handle_key(KeyEvent::from(KeyCode::Down));
            terminal
                .draw(|frame| {
                    picker.view(frame, frame.area());
                })
                .unwrap();
            assert_eq!(nav_positions(&picker, &terminal), rows);
            assert_eq!((picker.popup, picker.detail.popup), panes);
        }
        for (index, position) in rows.iter().enumerate() {
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: position.x,
                row: position.y,
                modifiers: KeyModifiers::NONE,
            });
            terminal
                .draw(|frame| {
                    picker.view(frame, frame.area());
                })
                .unwrap();
            assert_eq!(nav_positions(&picker, &terminal), rows);
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: position.x,
                row: position.y,
                modifiers: KeyModifiers::NONE,
            });
            assert!(picker.pending_revoke.is_none());
            assert_eq!(
                picker
                    .picker
                    .selected_item()
                    .and_then(|entry| entry.id.as_deref()),
                Some(NAV_LABELS[index])
            );
            picker.handle_key(KeyEvent::from(KeyCode::Esc));
            terminal
                .draw(|frame| {
                    picker.view(frame, frame.area());
                })
                .unwrap();
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "medium")]
    #[test_case(140; "wide")]
    fn detail_keyboard_and_pointer_scroll_do_not_navigate_or_search(width: u16) {
        let mut picker = navigation_picker();
        picker.handle_key(KeyEvent::from(KeyCode::Down));
        let mut terminal = Terminal::new(TestBackend::new(width, MANAGER_TEST_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let rows = nav_positions(&picker, &terminal);
        picker.handle_key(KeyEvent::from(KeyCode::Tab));
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        picker.handle_key(KeyEvent::from(KeyCode::PageDown));
        assert!(picker.scope_view.offset > 0);
        let selected = picker.picker.selected_index();
        picker.handle_paste(COMMAND);
        assert!(picker.picker.search_text().is_empty());
        let position = Position::new(picker.detail.popup.x + 1, picker.detail.popup.y + 1);
        picker.scroll_at(position, -1);
        assert_eq!(picker.picker.selected_index(), selected);
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        if width >= super::SIDE_BY_SIDE_WIDTH {
            assert_eq!(nav_positions(&picker, &terminal), rows);
        } else {
            assert!(!picker.detail.popup.is_empty());
        }
        picker.handle_key(KeyEvent::from(KeyCode::BackTab));
        picker.handle_key(KeyEvent::from(KeyCode::Down));
        assert_ne!(picker.picker.selected_index(), selected);
        assert_eq!(picker.detail.scroll.offset(), 0);
    }

    fn ready_discovery(partial: bool) -> DiscoveryState {
        let mut report = test_pattern_discovery_report(Vec::new());
        report.sample.sessions = 2;
        report.sample.rows = 4;
        report.recognition.retained_observations = 4;
        if partial {
            report.partial_reasons.push(DISCOVERY_TEST_PARTIAL.into());
        }
        DiscoveryState::Complete(Arc::new(PatternDiscoveryOutcome::Ready(report)))
    }

    #[test_case(DiscoveryState::Idle, DISCOVERY_IDLE; "idle")]
    #[test_case(DiscoveryState::Loading, DISCOVERY_LOADING; "loading")]
    #[test_case(DiscoveryState::Cancelled, DISCOVERY_CANCELLED; "cancelled")]
    #[test_case(ready_discovery(false), DISCOVERY_SAMPLE_COUNT; "ready_empty")]
    #[test_case(ready_discovery(true), DISCOVERY_TEST_PARTIAL; "partial")]
    #[test_case(DiscoveryState::Complete(Arc::new(PatternDiscoveryOutcome::Unavailable(DISCOVERY_TEST_ERROR))), DISCOVERY_TEST_ERROR; "unavailable")]
    fn discovery_states_explain_evidence_limits_and_empty_proposals(
        state: DiscoveryState,
        expected: &str,
    ) {
        let mut picker = PermissionsPicker::new();
        picker.open(Vec::new(), &[], &[], false, false);
        let label = state.label();
        picker.set_discovery(state);
        picker.show_discovery();
        let details = read_details(&mut picker, NARROW_WIDTH, SHORT_HEIGHT);
        for text in [
            label,
            expected,
            DISCOVERY_MINIMUM,
            DISCOVERY_WARNING,
            DISCOVERY_EMPTY,
            SUGGESTED_GUIDANCE,
        ] {
            assert!(
                details.contains(&compact(text)),
                "missing {text}: {details}"
            );
        }
        assert!(!picker.has_pending_confirmation());
    }

    #[test_case(40; "narrow_toolbar")]
    #[test_case(80; "medium_toolbar")]
    #[test_case(140; "wide_toolbar")]
    fn discovery_controls_are_explicit_and_loading_prevents_duplicate_scan(width: u16) {
        let mut picker = suggested_picker();
        let mut terminal = Terminal::new(TestBackend::new(width, MANAGER_TEST_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let refresh = picker.toolbar_hits.hit(0);
        assert!(refresh.width > 0);
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: refresh.x,
            row: refresh.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: refresh.x,
                row: refresh.y,
                modifiers: KeyModifiers::NONE
            }),
            PermissionsPickerAction::RefreshDiscovery
        ));
        picker.set_discovery(DiscoveryState::Loading);
        for code in [KeyCode::Char('r'), KeyCode::Char('d'), KeyCode::Char('s')] {
            assert!(matches!(
                picker.handle_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
                PermissionsPickerAction::Consumed
            ));
        }
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            PermissionsPickerAction::CancelDiscovery
        ));
        assert!(!picker.has_pending_confirmation());
        assert!(picker.suggestion_inspector.is_none());
    }

    #[test_case(ObservationProvenance::Native, Some(InvocationOutcome::Requested), NATIVE_ORIGIN, REQUESTED_OUTCOMES; "native_requests")]
    #[test_case(ObservationProvenance::Imported, Some(InvocationOutcome::Unknown), IMPORTED_ORIGIN, UNKNOWN_OUTCOMES; "imported_history")]
    #[test_case(ObservationProvenance::Imported, None, IMPORTED_ORIGIN, UNRECORDED_OUTCOMES; "imported_history_without_outcome_records")]
    fn suggestions_show_evidence_trust_and_execution_outcomes(
        provenance: ObservationProvenance,
        outcome: Option<InvocationOutcome>,
        origin: &str,
        outcomes: &str,
    ) {
        let mut candidate = suggestion();
        candidate.evidence.provenance = provenance.clone();
        candidate.evidence.outcomes = outcome.into_iter().map(|outcome| (outcome, 4)).collect();
        let mut picker = suggested_picker();
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION, &[candidate]);
        assert!(
            picker
                .picker
                .selected_item()
                .unwrap()
                .detail
                .contains(origin)
        );
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        let text = read_suggestion(&mut picker, NARROW_WIDTH, SHORT_HEIGHT);
        for expected in [origin, outcomes, SUGGESTED_OBSERVATIONS, SUGGESTED_SESSIONS] {
            assert!(text.contains(&compact(expected)), "missing {expected}");
        }
        for historical in [
            HISTORICAL_CONTEXT,
            HISTORICAL_ASSUMPTIONS,
            HISTORICAL_PROJECT,
        ] {
            assert_eq!(
                text.contains(&compact(historical)),
                provenance == ObservationProvenance::Imported,
                "{historical}",
            );
        }
    }

    #[test_case(24, "value-"; "narrow_ascii")]
    #[test_case(NARROW_WIDTH, "界é/"; "narrow_unicode")]
    #[test_case(25, "界e\u{301}/"; "narrow_combining_graphemes")]
    fn suggested_inspection_wraps_complete_long_templates_and_values(width: u16, word: &str) {
        let mut candidate = suggestion();
        let operation = format!("check-{}end-operation", "part-".repeat(LONG_WORD_REPEATS));
        let value = format!("{}end-value", word.repeat(LONG_WORD_REPEATS));
        candidate.definition.argv[1] = PatternToken::Exact {
            value: operation.clone(),
            role: ArgumentRole::Operation,
        };
        let slot = candidate.definition.slots[0].id;
        candidate.definition.slots[0].domain = ArgumentDomain::ObservedSet {
            values: [value.clone()].into(),
        };
        let tuple = BTreeMap::from([(slot, value.clone())]);
        candidate.definition.combinations = SlotCombinations::ObservedTuples {
            tuples: [tuple.clone()].into(),
        };
        candidate.evidence.tuples = vec![TupleSupport {
            values: tuple,
            support: candidate.evidence.support.clone(),
        }];
        let fingerprint = candidate.definition.fingerprint().unwrap();
        let mut picker = suggested_picker();
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION, &[candidate]);
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        let expected = compact(
            picker
                .picker
                .selected_item()
                .unwrap()
                .description
                .as_deref()
                .unwrap(),
        );
        let text = read_suggestion(&mut picker, width, SHORT_HEIGHT);
        assert!(
            text.contains(&expected),
            "incomplete inspector text: {text}"
        );
        assert!(text.contains(&format!("Commandtemplate:cargo{operation}<arg1>")));
        assert_eq!(text.matches(&value).count(), 2);
        for hidden in [OMITTED, "path_binding", "ObservedTuples", &fingerprint] {
            assert!(!text.contains(&compact(hidden)), "unexpected {hidden}");
        }
        assert!(
            picker
                .suggestion_inspector
                .as_ref()
                .unwrap()
                .scroll
                .offset()
                > 0
        );
        assert!(!picker.has_pending_confirmation());
    }

    #[test_case(KeyEvent::from(KeyCode::Esc); "escape_returns_to_list")]
    #[test_case(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL); "control_c_returns_to_list")]
    fn suggested_inspection_pages_and_mouse_scroll_without_editing_or_approving(close: KeyEvent) {
        let mut picker = suggested_picker();
        picker.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
        let mut terminal = Terminal::new(TestBackend::new(NARROW_WIDTH, SHORT_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        picker.handle_key(KeyEvent::from(KeyCode::PageDown));
        let inspector = picker.suggestion_inspector.as_ref().unwrap();
        let offset = inspector.scroll.offset();
        let popup = inspector.popup;
        assert!(offset > 0);
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: popup.x + 1,
            row: popup.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            picker
                .suggestion_inspector
                .as_ref()
                .unwrap()
                .scroll
                .offset(),
            offset - 1
        );
        assert!(picker.handle_paste(COMMAND));
        for key in [KeyCode::Char('y'), KeyCode::Enter] {
            assert!(matches!(
                picker.handle_key(KeyEvent::from(key)),
                PermissionsPickerAction::Consumed
            ));
        }
        assert!(picker.suggestion_inspector.is_some());
        assert!(!picker.has_pending_confirmation());
        picker.handle_key(close);
        picker.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
        assert_eq!(
            picker
                .suggestion_inspector
                .as_ref()
                .unwrap()
                .scroll
                .offset(),
            0
        );
    }

    #[test]
    fn suggestions_are_separate_read_only_human_templates() {
        let mut picker = suggested_picker();
        let mut terminal = Terminal::new(TestBackend::new(150, 36)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = compact(&buffer_text(terminal.backend().buffer()))
            + read_suggestion(&mut picker, 150, 36).as_str();
        for text in [
            SUGGESTED_SECTION,
            SUGGESTED_COMMAND,
            SUGGESTED_OBSERVATIONS,
            SUGGESTED_SESSIONS,
            SUGGESTED_GUIDANCE,
        ] {
            assert!(screen.contains(&compact(text)), "missing {text}");
        }
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PermissionsPickerAction::Consumed
        ));
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = buffer_text(terminal.backend().buffer());
        for text in [
            "Observed values (2): alpha, beta",
            "Combinations: 2 observed tuples only",
            "cargo test alpha",
            "Working directory:",
            SUGGESTED_GUIDANCE,
        ] {
            assert!(screen.contains(text), "missing {text}");
        }
        assert!(!screen.contains("path_binding"));
        assert!(!screen.contains(CONFIRM_REVOKE));
        assert!(picker.pending_revoke.is_none());
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PermissionsPickerAction::Consumed
        ));
        assert!(picker.detail_focused || picker.suggestion_inspector.is_some());
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Esc)),
            PermissionsPickerAction::Consumed
        ));
        assert!(picker.suggestion_inspector.is_none());
        assert!(picker.is_open());
    }

    #[test_case('d', false; "dismiss_from_list")]
    #[test_case('s', false; "snooze_from_list")]
    #[test_case('d', true; "dismiss_from_inspector")]
    #[test_case('s', true; "snooze_from_inspector")]
    fn suggestion_actions_are_explicit_and_bound_to_the_definition(key: char, inspect: bool) {
        let mut picker = suggested_picker();
        if inspect {
            picker.handle_key(KeyEvent::from(KeyCode::Enter));
        }
        let action = picker.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL));
        let target = match (key, action) {
            ('d', PermissionsPickerAction::DismissSuggestion(target)) => target,
            ('s', PermissionsPickerAction::SnoozeSuggestion(target)) => target,
            _ => panic!("expected a suggestion-only action"),
        };
        assert_eq!(target.project, Path::new(ROOT));
        assert_eq!(target.revision, SUGGESTION_REVISION);
        assert_eq!(
            target.definition_id,
            suggestion().definition.fingerprint().unwrap()
        );
        assert!(picker.pending_revoke.is_none());
    }

    #[test]
    fn untrusted_suggestion_labels_are_escaped_bounded_and_never_actions() {
        let mut candidate = suggestion();
        candidate.definition.name = "Trust project config \u{202e} Enter to grant".into();
        candidate.evidence.sources = [format!(
            "\x1b[31mRevoke\ny approve {}",
            "x".repeat(MAX_SUGGESTION_DETAIL_CHARS)
        )]
        .into();
        let rendered = suggestion_entry(Path::new(ROOT), SUGGESTION_REVISION, &candidate).unwrap();
        assert_eq!(rendered.tool, SUGGESTED_COMMAND);
        assert_eq!(rendered.section(), Some(SUGGESTED_SECTION));
        let description = rendered.description.as_ref().unwrap();
        assert!(description.contains("Source (untrusted label):"));
        assert!(description.contains("\\u{202e}"));
        assert!(description.contains("\\u{1b}"));
        assert!(!description.contains('\x1b'));
        assert!(!description.contains('\u{202e}'));
        assert!(description.contains(OMITTED));
        assert!(description.chars().count() <= MAX_SUGGESTION_DETAIL_CHARS + OMITTED.len());
        let mut picker = suggested_picker();
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION, &[candidate]);
        assert!(matches!(
            picker.map_action(PickerAction::Select(rendered)),
            PermissionsPickerAction::Consumed
        ));
        assert!(picker.detail_focused);
        for key in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Enter] {
            assert!(matches!(
                picker.handle_key(KeyEvent::from(key)),
                PermissionsPickerAction::Consumed
            ));
        }
        assert!(!picker.has_pending_confirmation());
    }

    #[test_case(false; "proposal_removed")]
    #[test_case(true; "context_changed")]
    fn proposal_refresh_clears_stale_inspection_without_revoking_rules(context_changed: bool) {
        let mut picker = suggested_picker();
        let rule_id = picker.entries[0].id.clone();
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        let candidates = if context_changed {
            vec![suggestion()]
        } else {
            Vec::new()
        };
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION + 1, &candidates);
        assert!(picker.suggestion_inspector.is_none());
        assert!(!picker.has_pending_confirmation());
        assert_eq!(picker.entries[0].id, rule_id);
        if !context_changed {
            assert!(
                picker
                    .entries
                    .iter()
                    .all(|entry| entry.suggestion.is_none())
            );
            assert!(matches!(
                picker.handle_key(KeyEvent::from(KeyCode::Enter)),
                PermissionsPickerAction::Consumed
            ));
            assert!(picker.pending_revoke.is_none());
            assert!(picker.discovery_mode());
            assert!(picker.picker.selected_item().is_none());
        }
    }

    #[test]
    fn suggestions_cannot_replace_a_pending_rule_revocation() {
        let mut picker = PermissionsPicker::new();
        let record = record();
        let id = record.id.clone();
        picker.open(vec![record], &[], &[], false, false);
        picker.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION, &[suggestion()]);
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            PermissionsPickerAction::Consumed
        ));
        assert!(
            matches!(picker.handle_key(KeyEvent::from(KeyCode::Enter)), PermissionsPickerAction::Revoke(selected) if selected == id)
        );
    }

    fn reviewed_record(tool: &str, input: Value, resources: &[&str]) -> PermissionRuleRecord {
        let request = PermissionRequest::from_legacy(
            "review".into(),
            ToolKey::native(tool),
            resources.iter().map(|value| (*value).into()).collect(),
            input,
            Path::new(ROOT),
            false,
        );
        let mut rule = request
            .option_rule("allow_exact", PermissionLifetime::Conversation)
            .unwrap();
        rule.effect = StructuredPermissionEffect::Deny;
        let review = review_for_rule(&request, &rule);
        PermissionRuleRecord::conversation_with_review(rule, Some(review)).unwrap()
    }

    fn record() -> PermissionRuleRecord {
        reviewed_record("bash", json!({"command": COMMAND}), &[COMMAND])
    }

    #[test]
    fn shows_structured_scope_and_requires_confirmed_revocation() {
        let record = record();
        let id = record.id.clone();
        let PermissionArgumentConstraint::Exact { digest } = &record.rule.arguments else {
            panic!("expected exact input")
        };
        let digest = digest.clone();
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = compact(&buffer_text(terminal.backend().buffer()))
            + read_details(&mut picker, 100, 24).as_str();
        assert!(screen.contains("bash"));
        assert!(screen.contains("[DENY]"));
        assert!(screen.contains("[CONVERSATION]"));
        assert!(screen.contains(&compact(COMMAND)));
        assert!(screen.contains("\"command\":"));
        assert!(screen.contains(&digest[..10]));
        assert!(!screen.contains(&id[..10]));

        let enter = KeyEvent::from(KeyCode::Enter);
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            PermissionsPickerAction::Consumed
        ));
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        assert!(buffer_text(terminal.backend().buffer()).contains(CONFIRM_REVOKE));
        assert!(matches!(
            picker.handle_key(enter),
            PermissionsPickerAction::Revoke(selected) if selected == id
        ));
    }

    #[test]
    fn shows_inactive_legacy_allows_for_review() {
        let mut picker = PermissionsPicker::new();
        picker.open(
            Vec::new(),
            &[PermissionReviewCandidate {
                source: PermissionSource::Project,
                kind: PermissionReviewKind::Rule,
                tool: Some(ToolKey::native("bash")),
                scope: Some("cargo *".into()),
            }],
            &[],
            false,
            false,
        );
        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = compact(&buffer_text(terminal.backend().buffer()))
            + read_details(&mut picker, 100, 16).as_str();
        assert!(screen.contains(&compact("needs review")));
        assert!(screen.contains(&compact("inactive allow rule")));
        assert!(screen.contains(&compact("project config")));
    }

    #[test]
    fn project_config_trust_requires_confirmation_and_can_be_cancelled() {
        let mut picker = PermissionsPicker::new();
        picker.open(Vec::new(), &[], &[], true, false);
        let backend = TestBackend::new(120, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = compact(&buffer_text(terminal.backend().buffer()))
            + read_details(&mut picker, 120, 18).as_str();
        assert!(screen.contains(&compact("Project permissions.toml")));
        assert!(screen.contains(&compact("shell allow patterns are inactive")));
        assert!(screen.contains(&compact("no authority has been granted")));
        assert!(screen.contains(&compact("Trust or revoke")));

        let enter = KeyEvent::from(KeyCode::Enter);
        assert!(matches!(
            picker.handle_key(enter),
            PermissionsPickerAction::Consumed
        ));
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = read_details(&mut picker, 120, 18);
        assert!(screen.contains(&compact(
            "Trust the exact project shell allow configuration"
        )));
        assert!(screen.contains(&compact("Edits invalidate trust")));

        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Esc)),
            PermissionsPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(enter),
            PermissionsPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Char('y'))),
            PermissionsPickerAction::TrustProjectConfig
        ));
    }

    #[test]
    fn mouse_selection_keeps_project_trust_confirmation_open() {
        let mut picker = PermissionsPicker::new();
        picker.open(Vec::new(), &[], &[], true, false);
        picker.picker.close();

        assert!(matches!(
            picker.map_action(PickerAction::Select(project_config_entry(
                ProjectConfigAction::Trust
            ))),
            PermissionsPickerAction::Consumed
        ));
        assert!(picker.is_open());
        assert_eq!(
            picker.pending_project_config_action,
            Some(ProjectConfigAction::Trust)
        );
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PermissionsPickerAction::TrustProjectConfig
        ));
    }

    #[test]
    fn command_pattern_is_visible_before_redacted_metadata() {
        let mut record = record();
        record.rule.resources[0].selector = PermissionResourceSelector::CommandPattern {
            pattern: COMMAND_PATTERN.into(),
        };
        record.rule.arguments = PermissionArgumentConstraint::Unconstrained;
        record.review = Some(review_from_candidates(
            &record.rule,
            "bash",
            None,
            &[],
            PermissionReviewSource::Approved,
        ));
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        let backend = TestBackend::new(70, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();

        let screen = compact(&buffer_text(terminal.backend().buffer()))
            + read_details(&mut picker, 70, 16).as_str();
        assert!(screen.contains(&compact(COMMAND_PATTERN)));
        assert!(screen.contains("TOKENPREFIX"));
    }

    #[test]
    fn trusted_project_config_can_be_revoked() {
        let mut picker = PermissionsPicker::new();
        picker.open(Vec::new(), &[], &[], false, true);

        let enter = KeyEvent::from(KeyCode::Enter);
        assert!(matches!(
            picker.handle_key(enter),
            PermissionsPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(enter),
            PermissionsPickerAction::RevokeProjectConfigTrust
        ));
    }

    #[test_case("file_read", json!({"filePath": FILE, "offset": 12}), FILE; "actual_path")]
    #[test_case("file_glob", json!({"path": ROOT, "pattern": PATTERN}), PATTERN; "actual_glob")]
    fn displays_named_first_party_input(tool: &str, input: Value, expected: &str) {
        let record = reviewed_record(tool, input.clone(), &[FILE]);
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = read_details(&mut picker, 120, 32);
        assert!(screen.contains(&compact(expected)), "{screen}");
        for key in input.as_object().unwrap().keys() {
            assert!(screen.contains(&format!("\"{key}\":")), "{screen}");
        }
    }

    #[test_case(false; "absent_review")]
    #[test_case(true; "typed_unavailable")]
    fn missing_review_is_explicit_without_hashes(typed: bool) {
        let mut record = record();
        record.review = typed.then(|| {
            review_from_candidates(
                &record.rule,
                "bash",
                None,
                &[],
                PermissionReviewSource::Unavailable,
            )
        });
        let id = record.id.clone();
        let rendered = entry(record);
        let description = rendered.description.unwrap();
        assert!(description.contains(&format!("Review: {UNAVAILABLE}")));
        assert!(description.contains(&format!("Input: {UNAVAILABLE}")));
        assert!(description.contains(&format!("(Command): {UNAVAILABLE}")));
        assert!(!description.contains(&id[..10]));
        assert_eq!(rendered.id.as_deref(), Some(id.as_str()));
    }

    #[test_case("file_read", json!({"filePath": FILE, "token": SECRET}); "secret_field")]
    #[test_case("bash", json!({"command": format!("deploy --token {SECRET} --target production")}); "command_token")]
    fn builder_redaction_survives_picker_rendering(tool: &str, input: Value) {
        let resource = input.get("command").and_then(Value::as_str).unwrap_or(FILE);
        let record = reviewed_record(tool, input.clone(), &[resource]);
        let rendered = entry(record);
        let description = rendered.description.unwrap();
        assert!(description.contains(REDACTED), "{description}");
        assert!(!description.contains(SECRET));
        assert!(!rendered.tool.contains(SECRET));
    }

    #[test_case(false; "selected_fields")]
    #[test_case(true; "unconstrained_input")]
    fn scope_description_does_not_turn_broad_authority_into_exact_input(broad: bool) {
        let input = json!({"path": ROOT, "pattern": PATTERN});
        let mut record = reviewed_record("file_glob", input.clone(), &[ROOT]);
        let pointers = vec!["/pattern".into()];
        record.rule.arguments = if broad {
            PermissionArgumentConstraint::Unconstrained
        } else {
            PermissionArgumentConstraint::SelectedDigest {
                digest: selected_input_digest(&input, &pointers).unwrap(),
                pointers,
            }
        };
        let review = review_from_candidates(
            &record.rule,
            "file_glob",
            Some(&input),
            &[ROOT.into()],
            PermissionReviewSource::Approved,
        );
        let record =
            PermissionRuleRecord::conversation_with_review(record.rule, Some(review)).unwrap();
        let id = record.id.clone();
        let rendered = entry(record);
        let description = rendered.description.unwrap();
        assert!(description.contains(if broad {
            "input unconstrained"
        } else {
            "selected input fields only"
        }));
        assert!(
            rendered
                .detail
                .starts_with(if broad { "[resource:" } else { "[selected:" })
        );
        assert_eq!(rendered.id.as_deref(), Some(id.as_str()));
    }

    #[test_case("\x1b[31m\n\r\t"; "terminal_controls")]
    #[test_case("\u{202e}\u{2066}"; "direction_controls")]
    fn review_display_is_escaped_and_bounded(controls: &str) {
        let text = format!("{controls}{}", "界".repeat(MAX_DISPLAY_CHARS));
        let safe = display_text(&text, MAX_FIELD_CHARS);
        assert!(!safe.contains(controls));
        assert!(!safe.chars().any(char::is_control));
        assert!(safe.ends_with(OMITTED));
        assert!(safe.chars().count() <= MAX_FIELD_CHARS + OMITTED.len());
        let mut record = record();
        let review = record.review.as_mut().unwrap();
        review.tool = text.clone();
        review.authority = text.clone();
        review.input = Some(json!({text.clone(): text.clone()}));
        review.resources[0].value = Some(text);
        record.rule.resources[0].kind = PermissionResourceKind::Custom {
            name: controls.into(),
        };
        let rendered = entry(record);
        let description = rendered.description.unwrap();
        assert!(!description.contains(controls));
        assert!(description.chars().count() <= MAX_DISPLAY_CHARS + OMITTED.len());
        assert!(rendered.tool.chars().count() <= MAX_DISPLAY_CHARS + OMITTED.len());
    }

    #[test_case(KeyCode::Enter; "enter_confirmation")]
    #[test_case(KeyCode::Char('y'); "yes_confirmation")]
    fn cancelled_revocation_requires_fresh_confirmation(confirm: KeyCode) {
        let record = record();
        let id = record.id.clone();
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            PermissionsPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Esc)),
            PermissionsPickerAction::Consumed
        ));
        assert!(picker.pending_revoke.is_none());
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            PermissionsPickerAction::Consumed
        ));
        assert!(
            matches!(picker.handle_key(KeyEvent::from(confirm)), PermissionsPickerAction::Revoke(selected) if selected == id)
        );
    }

    #[test]
    fn mouse_selection_restores_the_selected_entry() {
        let record = record();
        let selected_id = record.id.clone();
        let selected = entry(record.clone());
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], true, false);

        assert!(matches!(
            picker.map_action(PickerAction::Select(selected)),
            PermissionsPickerAction::Consumed
        ));
        assert_eq!(
            picker
                .picker
                .selected_item()
                .and_then(|entry| entry.id.as_ref()),
            Some(&selected_id)
        );
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PermissionsPickerAction::Consumed
        ));
        assert!(picker.pending_revoke.is_none());
    }

    #[test_case('n'; "new")]
    #[test_case('e'; "edit")]
    #[test_case('u'; "duplicate")]
    #[test_case('b'; "copy")]
    fn management_shortcuts_return_typed_requests_without_mutation(shortcut: char) {
        let record = record();
        let expected = record.clone();
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        let action = picker.handle_key(KeyEvent::new(
            KeyCode::Char(shortcut),
            KeyModifiers::CONTROL,
        ));
        match action {
            PermissionsPickerAction::Editor(super::EditorEvent::Begin(
                super::EditorLaunch::New,
            )) => assert_eq!(shortcut, 'n'),
            PermissionsPickerAction::Editor(super::EditorEvent::Begin(
                super::EditorLaunch::Edit(record),
            )) => {
                assert_eq!(shortcut, 'e');
                assert_eq!(*record, expected);
            }
            PermissionsPickerAction::Editor(super::EditorEvent::Begin(
                super::EditorLaunch::Duplicate(record),
            )) => {
                assert_eq!(shortcut, 'u');
                assert_eq!(*record, expected);
            }
            PermissionsPickerAction::Editor(super::EditorEvent::Begin(
                super::EditorLaunch::Copy { source, draft },
            )) => {
                assert_eq!(shortcut, 'b');
                assert_eq!(*source, expected);
                assert!(draft.is_none());
            }
            _ => panic!("expected a typed editor launch"),
        }
        assert!(picker.pending_revoke.is_none());
        assert!(picker.editor.is_none());
    }

    #[test_case(false; "proposal_keyboard")]
    #[test_case(true; "proposal_mouse")]
    fn discover_create_passes_candidate_evidence_to_editor(mouse: bool) {
        let mut picker = suggested_picker();
        export_buffer(&mut picker, 80, 24);
        let action = if mouse {
            let hit = picker.toolbar_hits.hit(3);
            assert!(!hit.is_empty());
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.x,
                row: hit.y,
                modifiers: KeyModifiers::NONE,
            });
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: hit.x,
                row: hit.y,
                modifiers: KeyModifiers::NONE,
            })
        } else {
            picker.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        };
        let PermissionsPickerAction::Editor(super::EditorEvent::Begin(
            super::EditorLaunch::Discover(candidate),
        )) = action
        else {
            panic!("expected discovery draft");
        };
        assert_eq!(*candidate, suggestion());
        assert!(picker.pending_revoke.is_none());
    }

    #[test_case(false; "conditions")]
    #[test_case(true; "identity")]
    fn two_hundred_forty_rules_keep_whole_inventory_cells_when_scope_changes(identity: bool) {
        let mut picker = export_picker("discovery-long-list");
        picker.set_mode(PermissionsMode::Rules);
        assert_eq!(picker.visible_entries().len(), EXPORT_LONG_GRANTS);
        let before = export_buffer(&mut picker, 140, 32);
        let anchors = export_list_anchors(&picker, &export_rows(&before), &EXPORT_LONG_ANCHORS);
        picker.scope_view.disclosure = Some(if identity {
            Disclosure::Identity
        } else {
            Disclosure::Conditions
        });
        picker.scope_view.scroll(4);
        let after = export_buffer(&mut picker, 140, 32);
        assert_eq!(
            export_list_anchors(&picker, &export_rows(&after), &EXPORT_LONG_ANCHORS),
            anchors
        );
        for y in before.area.y..before.area.bottom() {
            for x in before.area.x..before.area.right() {
                if picker.picker.contains(Position::new(x, y)) {
                    assert_eq!(before[(x, y)], after[(x, y)]);
                }
            }
        }
    }

    #[test_case(40, false; "narrow_copy")]
    #[test_case(80, false; "normal_copy")]
    #[test_case(140, false; "wide_copy")]
    #[test_case(40, true; "narrow_create")]
    #[test_case(80, true; "normal_create")]
    #[test_case(140, true; "wide_create")]
    fn last_toolbar_action_is_mouse_reachable(width: u16, discover: bool) {
        let mut picker = if discover {
            suggested_picker()
        } else {
            let mut picker = PermissionsPicker::new();
            picker.open(vec![record()], &[], &[], false, false);
            picker
        };
        export_buffer(&mut picker, width, MANAGER_TEST_HEIGHT);
        let hit = picker.toolbar_hits.hit(if discover { 3 } else { 4 });
        assert!(!hit.is_empty());
        let event = |kind| MouseEvent {
            kind,
            column: hit.x,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        };
        picker.handle_mouse(event(MouseEventKind::Down(MouseButton::Left)));
        let action = picker.handle_mouse(event(MouseEventKind::Up(MouseButton::Left)));
        if discover {
            assert!(matches!(
                action,
                PermissionsPickerAction::Editor(super::EditorEvent::Begin(
                    super::EditorLaunch::Discover(_)
                ))
            ));
        } else {
            assert!(matches!(
                action,
                PermissionsPickerAction::Editor(super::EditorEvent::Begin(
                    super::EditorLaunch::Copy { .. }
                ))
            ));
        }
        assert!(picker.pending_revoke.is_none());
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn wheel_scrolls_typed_scope_instead_of_legacy_detail(width: u16) {
        let mut record = record();
        record.review.as_mut().unwrap().resources[0].value =
            Some(COMMAND.repeat(EXPORT_LONG_GRANTS));
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        picker.scope_view.disclosure = Some(Disclosure::Evidence);
        export_buffer(&mut picker, width, MANAGER_TEST_HEIGHT);
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: picker.detail.popup.x,
            row: picker.detail.popup.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(picker.scope_view.offset, 1);
        assert_eq!(picker.detail.scroll.offset(), 0);
    }
}
