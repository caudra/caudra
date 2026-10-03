use caudra_agent::permissions::{
    ActivePolicyRule, PermissionArgumentConstraint, PermissionResourceKind, PermissionRuleRecord,
    PermissionSubject, RuleOrigin, StructuredPermissionEffect, VerifiedLocalSourceLocator,
    pattern_recognition::{
        CandidateEvidence, MAX_RECOGNIZER_SUGGESTIONS, ObservationProvenance, PatternCandidate,
        RecognizerLimits,
    },
    review::{command_template_values, slot_label},
};
use caudra_config::{
    Effect, PermissionReviewCandidate, PermissionReviewKind, PermissionRule, PermissionSource,
    ToolKey,
};
use caudra_grab::grab_scope;
use caudra_storage::permission_patterns::{
    ArgumentDomain, ObservedTuple, OptionLikePolicy, PatternDefinition, PatternToken,
    SlotCombinations,
};
use caudra_workbench::keys::{LIST_FIRST, LIST_LAST};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use serde_json::Value;
use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

use crate::components::command_text::{code_spans_in, pattern_spans};
use crate::components::keybindings::Bind;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::{CHROME_LINES, FooterHits, FooterLine, Modal};
use crate::components::permission_prompt::{
    lifetime_phrase, origin_word, pattern_widened, rule_names_commands, rule_phrase, rule_summary,
    tilde, tool_words,
};
use crate::components::permission_scope::editor::{EditorEvent, EditorLaunch, ScopeEditor};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{
    Hint, HintBar, ModalScroll, Overlay, counted, escape_terminal_controls, format_elapsed,
    format_iec_bytes,
};
use crate::theme;
use crate::{PatternDiscoveryOutcome, PatternDiscoveryReport};

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
const EMPTY_DISCOVERY: &str = "No suggestions. See the scan overview.";
const RULES_TITLE: &str = " Rules · grants & policies ";
const DISCOVER_TITLE: &str = " Discover ";
const RULE_PANE_TITLE: &str = " Selected permission ";
const SUGGESTION_PANE_TITLE: &str = " Suggestion ";
const OVERVIEW_PANE_TITLE: &str = " Scan overview ";
const MAX_DISPLAY_CHARS: usize = 2048;
const MAX_FIELD_CHARS: usize = 256;
const MAX_DISPLAY_ITEMS: usize = 16;
const MAX_INPUT_DEPTH: usize = 4;
const OMITTED: &str = "[omitted: display limit]";
const SUGGESTED_SECTION: &str = "Suggested";
const PROJECT_CONFIG_SECTION: &str = "Project configuration";
const STORED_SECTION: &str = "Stored permissions";
const POLICY_SECTION: &str = "Active policy · read-only";
const REVIEW_SECTION: &str = "Needs review · inactive";
/// The one place the manager says a suggestion grants nothing yet.
const NOT_ACTIVE: &str = "Not active. Ctrl-E drafts a permission you review before saving.";
const COVERS: &str = "Covers these commands";
const COVERS_SUCH_AS: &str = "Covers commands such as";
const MAX_COVERED_COMMANDS: usize = 6;
const STILL_ASKS: &str = "Still asks for";
const OTHER_VALUES: &str = "other values";
const OPTION_VALUES: &str = "values that start with -";
const OTHER_COMBINATIONS: &str = "other combinations of these values";
const ALWAYS_ASKS: [&str; 3] = [
    "extra arguments, redirects and expansions",
    "the same commands run from another folder",
    "any other command on the same line",
];
const LIST_INDENT: &str = "  ";
const COUNT_GAP: &str = "  ";
const UNVERIFIED_SOURCE: &str = "Caudra cannot verify this history.";
const SCROLLBAR_WIDTH: u16 = 1;
const MANAGER_SIZE_PERCENT: u16 = 95;
const SIDE_BY_SIDE_WIDTH: u16 = 110;
/// Rows are sentences with a place and an origin, so the list takes the
/// larger share and the short summary beside it the rest.
const LIST_PERCENT: u16 = 55;
const PANE_GAP: u16 = 1;
const MIN_CHROME_HEIGHT: u16 = 16;
const DISCOVERY_UNVERIFIED: &str = "Saved history shows what was asked for, not whether it worked. Imported history is unverified.";
const DISCOVERY_EMPTY: &str = "No suggestions. Unsupported and sensitive commands are left out, dismissed and snoozed ones stay hidden, and a scan reads only part of your history.";
const DISCOVERY_IDLE: &str =
    "Ctrl-R scans saved history for commands you run often. A scan changes no permissions.";
const DISCOVERY_LOADING: &str = "Reading saved history in the background. Ctrl-X cancels, and your permissions stay as they are.";
const DISCOVERY_CANCELLED: &str = "Scan cancelled. Nothing it found is shown. Ctrl-R scans again.";
const DISCOVERY_CLOSING: &str = "Suggestions allow nothing until you create a permission from one.";
const SCAN_COMPLETE: &str = "Scan complete.";
const SCAN_PARTIAL: &str = "Partial scan.";
const STOPPED_EARLY: &str = "Stopped early: ";
const MILLIS_PER_SECOND: u64 = 1_000;
const ROW: &str = "row";
const TOOL_CALL: &str = "tool call";
const SUGGESTION: &str = "suggestion";
const SESSION: &str = "session";
const READ_ONLY_POLICY: &str =
    "It can't be changed here. Change it where it was set, such as your config or the plugin.";
const EDIT_AT_SOURCE: &str = "Ctrl-E opens the file it comes from.";
const INACTIVE_ALLOW: &str = "Caudra no longer applies it. Approve the next matching request again, or remove the entry from the config.";
const CONFIRM_REVOKE_MESSAGE: &str =
    "Revoke this permission? Press Enter or y to confirm, or Esc to cancel.";
const REVOKE_CAUTION: &str = "Calls already running are not stopped. Revoking a Deny or Ask rule can let more run without asking.";
const TRUST_MESSAGE: &str = "Trust this project's permissions.toml? Its shell allow rules start to apply, and any edit to the file withdraws trust. Press Enter or y to confirm, or Esc to cancel.";
const REVOKE_TRUST_MESSAGE: &str = "Stop trusting this project's permissions.toml? Its shell allow rules stop applying. Press Enter or y to confirm, or Esc to cancel.";
const PROJECT_CONFIG: &str = "Project permissions.toml";
const TRUSTED_CONFIG: &str = "Shell allow patterns are active · trust can be revoked";
const UNTRUSTED_CONFIG: &str =
    "Shell allow patterns are inactive · nothing is granted until you trust them";
const TRUST_REVIEW: &str = "Enter reviews the trust change before you confirm it.";
const TRUST_HINT: &str = "Trust or revoke";
const TRUSTED: &str = "trusted";
const NOT_TRUSTED: &str = "not trusted";
const ALLOW: &str = "Allow";
const ASK: &str = "Ask";
const DENY: &str = "Deny";
/// The widest effect word, so every row's scope starts in one column.
const EFFECT_WIDTH: usize = 5;
/// The widest place a row names, `this conversation`, so origins line up.
const PLACE_WIDTH: usize = 17;
const EFFECT_GAP: &str = "  ";
const ALWAYS: &str = "always";
const INACTIVE: &str = "inactive";
const REVOKED: &str = "revoked";
const OTHER_PROJECT: &str = "other project";
const YOU: &str = "you";
const CONFIG: &str = "config";
const ANY_TOOL: &str = "any tool";
const ANYTHING: &str = "anything";
const TOOL_PREFIX: &str = "Tool: ";
const FIXED_INPUT: &str = "Fixed input: ";
const FIELD_PATH: &str = "pointer";
const FIELD_VALUE: &str = "value";
const NOT_SET: &str = "not set";
const NAMED: &str = "Named: ";
const ADDED_BY_YOU: &str = " · added by you";
const REVOKED_NOTE: &str = "Revoked; it no longer applies";
const FROM_CONFIG: &str = "Set in your permissions configuration.";
const BUILT_IN_RULE: &str = "Built into Caudra.";
const FROM_PLUGIN: &str = "Added by a plugin you trust.";
const CONVERSATION: &str = "conversation";

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
    Copy(String),
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
        origin: RuleOrigin,
        rule: PermissionRule,
        locator: Option<VerifiedLocalSourceLocator>,
    },
    Review(PermissionReviewCandidate),
    ProjectConfig,
}

/// One row: a sentence of effect and scope, with where it applies and who
/// added it on the right.
#[derive(Clone, PartialEq)]
struct PermissionEntry {
    source: PickerEntry,
    id: Option<String>,
    label: String,
    status: Option<String>,
    read_only_policy: bool,
    project_config_action: Option<ProjectConfigAction>,
    suggestion: Option<SuggestedPatternTarget>,
}

impl PermissionEntry {
    fn effect(&self) -> Option<StructuredPermissionEffect> {
        match &self.source {
            PickerEntry::Stored(record) => Some(record.rule.effect.clone()),
            PickerEntry::Policy { rule, .. } => Some(configured_effect(&rule.effect)),
            PickerEntry::Review(_) => Some(StructuredPermissionEffect::Allow),
            PickerEntry::Discovered(_) | PickerEntry::ProjectConfig => None,
        }
    }
}

impl PickerItem for PermissionEntry {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        self.status.as_deref()
    }

    fn lead(&self) -> Option<(usize, Style)> {
        let effect = self.effect()?;
        let theme = theme::current();
        let style = match (&self.source, &effect) {
            (PickerEntry::Review(_), _) => theme.tool_dim,
            (_, StructuredPermissionEffect::Allow) => theme.tool_success,
            (_, StructuredPermissionEffect::Ask) => theme.tool_warning,
            (_, StructuredPermissionEffect::Deny) => theme.tool_error,
        };
        Some((effect_word(&effect).len(), style))
    }

    /// A stored rule that names one command draws it as shell after its
    /// effect word, and a suggestion's template is shell throughout.
    fn label_colours(&self) -> Option<Vec<Span<'static>>> {
        let record = match &self.source {
            PickerEntry::Stored(record) => record,
            PickerEntry::Discovered(_) => {
                return Some(pattern_spans(
                    &self.label,
                    Style::default(),
                    theme::current().accent,
                ));
            }
            _ => return None,
        };
        if !rule_names_commands(&record.rule, record.review.as_ref()) {
            return None;
        }
        let (lead, style) = self.lead()?;
        let phrase = EFFECT_WIDTH + EFFECT_GAP.len();
        let mut spans = vec![
            Span::styled(self.label.get(..lead)?.to_owned(), style),
            Span::raw(self.label.get(lead..phrase)?.to_owned()),
        ];
        spans.extend(pattern_spans(
            self.label.get(phrase..)?,
            Style::default(),
            theme::current().accent,
        ));
        Some(spans)
    }

    fn section(&self) -> Option<&str> {
        Some(if self.suggestion.is_some() {
            SUGGESTED_SECTION
        } else if self.project_config_action.is_some() {
            PROJECT_CONFIG_SECTION
        } else if self.id.is_some() {
            STORED_SECTION
        } else if self.read_only_policy {
            POLICY_SECTION
        } else {
            REVIEW_SECTION
        })
    }
}

pub(crate) struct PermissionsPicker {
    picker: ListPicker<PermissionEntry>,
    entries: Vec<PermissionEntry>,
    pending_revoke: Option<String>,
    pending_project_config_action: Option<ProjectConfigAction>,
    discovery: DiscoveryState,
    mode: PermissionsMode,
    other_selection: usize,
    discovery_view: bool,
    detail_focused: bool,
    detail: DetailScroll,
    notice: Option<String>,
    popup: Rect,
    toolbar_hits: FooterHits,
    tabs_hits: FooterHits,
    footer: HintBar,
    editor: Option<ScopeEditor>,
    current_project: Option<PathBuf>,
    project_filter: ProjectFilter,
}

/// The right-hand pane's scroll position, its bar, and where it was drawn.
struct DetailScroll {
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    popup: Rect,
}

impl DetailScroll {
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
}

impl PermissionsPicker {
    pub(crate) fn new() -> Self {
        let mut picker = ListPicker::new().with_width_percent(100).docked();
        picker.set_empty_text(EMPTY);
        picker.set_footer_builder(footer);
        Self {
            picker,
            entries: Vec::new(),
            pending_revoke: None,
            pending_project_config_action: None,
            discovery: DiscoveryState::Idle,
            mode: PermissionsMode::Rules,
            other_selection: 0,
            discovery_view: false,
            detail_focused: false,
            detail: DetailScroll::new(),
            notice: None,
            popup: Rect::default(),
            toolbar_hits: FooterHits::default(),
            tabs_hits: FooterHits::default(),
            footer: HintBar::default(),
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
            .chain(
                rules
                    .into_iter()
                    .map(|record| entry(Arc::new(record), self.current_project.as_deref())),
            )
            .chain(review_candidates.iter().map(review_entry))
            .chain(effective_policy.iter().map(policy_entry))
            .collect();
        self.pending_revoke = None;
        self.pending_project_config_action = None;
        self.notice = None;
        self.detail = DetailScroll::new();
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
            } else if key::QUIT.matches(key) {
                return PermissionsPickerAction::Close;
            } else {
                self.detail.scroll.handle_key(key);
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
        let moves_list = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) || LIST_FIRST.matches(key)
            || LIST_LAST.matches(key);
        if moves_list && self.discovery_view && self.picker.selected_item().is_some() {
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
        // Ahead of the confirmation gate: the hint is a key press, and the
        // key path already knows what Esc means while a confirmation is up.
        if let Some(key) = self.footer.handle_mouse(event) {
            return self.handle_key(key);
        }
        if self.has_pending_confirmation() {
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
        if self.detail_focused || self.has_pending_confirmation() {
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
        if self.has_pending_confirmation() {
            return;
        }
        if self.detail_focused {
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
        if self.has_pending_confirmation() {
            self.scroll(delta);
        } else if self.detail.popup.contains(position) {
            self.detail.scroll.scroll(delta);
        } else if self.picker.contains(position) {
            let selected = self.picker.selected_index();
            self.picker.scroll(delta);
            self.selection_changed(selected);
        }
    }

    pub(crate) fn contains(&self, position: Position) -> bool {
        self.popup.contains(position)
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("permissions_picker", area);
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
        let mut status_line = vec![Span::styled(
            if self.mode == PermissionsMode::Discover {
                format!(
                    "{} · {}",
                    self.discovery.label(),
                    counted(self.suggestion_count(), SUGGESTION)
                )
            } else {
                format!("Grants & policies · Discovery: {}", self.discovery.label())
            },
            theme.panel_title,
        )];
        if let Some(report) = self.report() {
            status_line.push(Span::styled(
                format!(" · {} scanned", counted(report.sample.sessions, SESSION)),
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
                Constraint::Percentage(LIST_PERCENT),
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
        grab_scope!("permissions_picker_detail", area);
        self.detail.popup = area;
        if area.is_empty() {
            return;
        }
        let theme = theme::current();
        let block = Block::bordered()
            .title(if self.mode == PermissionsMode::Discover {
                if self.discovery_view || self.picker.selected_item().is_none() {
                    OVERVIEW_PANE_TITLE
                } else {
                    SUGGESTION_PANE_TITLE
                }
            } else {
                RULE_PANE_TITLE
            })
            .border_style(if self.detail_focused {
                theme.panel_title
            } else {
                theme.panel_border
            });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = if let Some(notice) = &self.notice {
            notice
                .lines()
                .map(|line| Line::styled(line.to_owned(), theme.panel_title))
                .collect()
        } else if self.mode == PermissionsMode::Discover
            && (self.discovery_view || self.picker.selected_item().is_none())
        {
            self.discovery_lines()
        } else if let Some(entry) = self.picker.selected_item() {
            summary_lines(entry, self.current_project.as_deref())
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

    fn suggestion_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.suggestion.is_some())
            .count()
    }

    fn report(&self) -> Option<&PatternDiscoveryReport> {
        match &self.discovery {
            DiscoveryState::Complete(outcome) => match outcome.as_ref() {
                PatternDiscoveryOutcome::Ready(report) => Some(report),
                PatternDiscoveryOutcome::Unavailable(_) => None,
            },
            _ => None,
        }
    }

    /// The scan overview: what the last scan did, what makes a suggestion,
    /// the limits and caveats in muted text, and that nothing is granted.
    fn discovery_lines(&self) -> Vec<Line<'static>> {
        let theme = theme::current();
        let mut lines = match &self.discovery {
            DiscoveryState::Idle => vec![Line::from(DISCOVERY_IDLE)],
            DiscoveryState::Loading => vec![Line::from(DISCOVERY_LOADING)],
            DiscoveryState::Cancelled => vec![Line::from(DISCOVERY_CANCELLED)],
            DiscoveryState::Complete(outcome) => match outcome.as_ref() {
                PatternDiscoveryOutcome::Unavailable(reason) => {
                    vec![Line::styled(*reason, theme.error)]
                }
                PatternDiscoveryOutcome::Ready(report) => {
                    scan_lines(report, self.suggestion_count())
                }
            },
        };
        let report = self.report();
        let min_sessions = report.map_or(RecognizerLimits::default().min_sessions, |report| {
            report.recognizer_limits.min_sessions
        });
        lines.extend([
            Line::default(),
            Line::from(format!(
                "Saved history yields a suggestion when similar commands appear in at least {}. Commands you repeat in this conversation can also become suggestions.",
                counted(min_sessions, SESSION)
            )),
        ]);
        if self.suggestion_count() == 0 {
            lines.push(Line::from(DISCOVERY_EMPTY));
        }
        lines.push(Line::default());
        if let Some(report) = report {
            lines.push(Line::styled(scan_limits(report), theme.item_desc));
        }
        lines.extend([
            Line::styled(DISCOVERY_UNVERIFIED, theme.item_desc),
            Line::default(),
            Line::styled(DISCOVERY_CLOSING, theme.panel_title),
        ]);
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
            PickerAction::Copy(text) => PermissionsPickerAction::Copy(text),
        }
    }

    fn confirm_project_config_action(&mut self, action: ProjectConfigAction) {
        self.pending_project_config_action = Some(action);
        self.notice = Some(
            match action {
                ProjectConfigAction::Trust => TRUST_MESSAGE,
                ProjectConfigAction::RevokeTrust => REVOKE_TRUST_MESSAGE,
            }
            .into(),
        );
        self.detail.scroll.reset();
    }

    fn confirm_revoke(&mut self, id: String) {
        self.pending_revoke = Some(id);
        self.notice = Some(format!("{CONFIRM_REVOKE_MESSAGE}\n{REVOKE_CAUTION}"));
        self.detail.scroll.reset();
    }

    fn show_read_only(&mut self) {
        self.notice = Some(READ_ONLY_POLICY.into());
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
            for permission in &mut self.entries {
                if let PickerEntry::Stored(record) = &permission.source {
                    *permission = entry(Arc::clone(record), self.current_project.as_deref());
                }
            }
            if self.mode == PermissionsMode::Rules && self.picker.is_open() {
                self.picker.replace_items(self.visible_entries());
            }
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
                self.show_read_only();
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
            self.show_read_only();
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
        self.toolbar_hits = FooterHits::default();
        self.tabs_hits = FooterHits::default();
        self.picker.close();
    }
}

fn project_config_entry(action: ProjectConfigAction) -> PermissionEntry {
    PermissionEntry {
        source: PickerEntry::ProjectConfig,
        id: None,
        label: PROJECT_CONFIG.into(),
        status: Some(
            match action {
                ProjectConfigAction::Trust => NOT_TRUSTED,
                ProjectConfigAction::RevokeTrust => TRUSTED,
            }
            .into(),
        ),
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
    Some(PermissionEntry {
        source: PickerEntry::Discovered(Arc::new(candidate.clone())),
        id: None,
        label: suggestion_template(definition, None),
        status: Some(format!(
            "{}× · {}",
            candidate.evidence.support.observations,
            places(&candidate.evidence)
        )),
        read_only_policy: false,
        project_config_action: None,
        suggestion: Some(SuggestedPatternTarget {
            project: project.to_path_buf(),
            revision,
            definition_id,
        }),
    })
}

/// Where a suggestion was seen: `2 conversations` of Caudra's own, or
/// `2 sessions` of history it did not record.
fn places(evidence: &CandidateEvidence) -> String {
    counted(
        evidence.support.independent_sessions,
        if matches!(evidence.provenance, ObservationProvenance::Native) {
            CONVERSATION
        } else {
            SESSION
        },
    )
}

/// A suggestion as the pane reads it: the template and what its slots stand
/// for, the commands it covers, what still asks, where it was seen, and that
/// it grants nothing yet.
fn suggestion_lines(candidate: &PatternCandidate) -> Vec<Line<'static>> {
    let theme = theme::current();
    let definition = &candidate.definition;
    let evidence = &candidate.evidence;
    let shell = |command: &str| pattern_spans(command, Style::default(), theme.accent);
    let mut lines = vec![Line::from(shell(&suggestion_template(definition, None)))];
    if let Some(values) = command_template_values(definition) {
        lines.push(Line::from(display_text(&values, MAX_DISPLAY_CHARS)));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        if pattern_widened(definition) {
            COVERS_SUCH_AS
        } else {
            COVERS
        },
        theme.panel_title,
    ));
    let mut tuples: Vec<_> = evidence.tuples.iter().collect();
    tuples.sort_by_key(|tuple| Reverse(tuple.support.observations));
    let covered: Vec<_> = tuples
        .iter()
        .take(MAX_COVERED_COMMANDS)
        .map(|tuple| {
            (
                suggestion_template(definition, Some(&tuple.values)),
                tuple.support.observations,
            )
        })
        .collect();
    let width = covered
        .iter()
        .map(|(command, _)| command.width())
        .max()
        .unwrap_or_default();
    for (command, count) in &covered {
        let mut spans = vec![Span::raw(LIST_INDENT)];
        spans.extend(shell(command));
        spans.push(Span::styled(
            format!("{}{COUNT_GAP}{count}×", " ".repeat(width - command.width())),
            theme.item_desc,
        ));
        lines.push(Line::from(spans));
    }
    if tuples.len() > MAX_COVERED_COMMANDS {
        lines.push(Line::from(format!(
            "{LIST_INDENT}and {} more",
            tuples.len() - MAX_COVERED_COMMANDS
        )));
    }
    lines.push(Line::default());
    lines.push(Line::styled(STILL_ASKS, theme.panel_title));
    lines.extend(
        still_asks(definition)
            .into_iter()
            .map(|item| Line::from(format!("{LIST_INDENT}{item}"))),
    );
    lines.push(Line::default());
    let mut seen = format!(
        "Seen {}× in {}, run from {}.",
        evidence.support.observations,
        places(evidence),
        tilde(&definition.context.effective_workdir)
    );
    if !matches!(evidence.provenance, ObservationProvenance::Native) {
        seen.push(' ');
        seen.push_str(UNVERIFIED_SOURCE);
    }
    lines.push(Line::styled(
        display_text(&seen, MAX_DISPLAY_CHARS),
        theme.item_desc,
    ));
    lines.push(Line::styled(NOT_ACTIVE, theme.tool_warning));
    lines
}

/// What a template leaves to a prompt: values and combinations it was not
/// shown, and whatever no template covers.
fn still_asks(definition: &PatternDefinition) -> Vec<&'static str> {
    let listed = |domain: &ArgumentDomain| {
        matches!(
            domain,
            ArgumentDomain::ObservedSet { .. } | ArgumentDomain::Exact { .. }
        )
    };
    let mut items = Vec::new();
    if definition.slots.iter().any(|slot| listed(&slot.domain)) {
        items.push(OTHER_VALUES);
    }
    if definition
        .slots
        .iter()
        .any(|slot| !listed(&slot.domain) && slot.option_like == OptionLikePolicy::Reject)
    {
        items.push(OPTION_VALUES);
    }
    if definition.slots.len() > 1
        && matches!(
            definition.combinations,
            SlotCombinations::ObservedTuples { .. }
        )
    {
        items.push(OTHER_COMBINATIONS);
    }
    items.extend(ALWAYS_ASKS);
    items
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
                .unwrap_or_else(|| slot_label(definition, *id)),
        })
        .collect();
    if definition.argv.len() > MAX_DISPLAY_ITEMS {
        words.push(OMITTED.into());
    }
    bounded_text(&words.join(" "), MAX_DISPLAY_CHARS)
}

/// What a finished scan read, looked at and found, in sentences.
fn scan_lines(report: &PatternDiscoveryReport, shown: usize) -> Vec<Line<'static>> {
    let theme = theme::current();
    let mut lines = vec![
        Line::styled(
            if report.partial_reasons.is_empty() {
                SCAN_COMPLETE
            } else {
                SCAN_PARTIAL
            },
            theme.panel_title,
        ),
        Line::from(format!(
            "Read {} of up to {}: {} of history ({} of {}).",
            report.sample.sessions,
            counted(report.history_limits.max_sessions, SESSION),
            counted(report.sample.rows, ROW),
            iec_bytes(report.sample.bytes),
            iec_bytes(report.history_limits.max_bytes),
        )),
        Line::from(format!(
            "Looked at {} ({} of commands) and kept {} to compare.",
            counted(report.calls, TOOL_CALL),
            iec_bytes(report.analysis_bytes),
            report.recognition.retained_observations,
        )),
        Line::from(format!(
            "Found {}, {shown} shown.",
            counted(report.candidates.len(), SUGGESTION)
        )),
    ];
    lines.extend(
        report
            .partial_reasons
            .iter()
            .map(|reason| Line::styled(format!("{STOPPED_EARLY}{reason}"), theme.error)),
    );
    lines
}

fn scan_limits(report: &PatternDiscoveryReport) -> String {
    format!(
        "A scan reads at most {} of up to {} each and {} with {} of commands, keeps up to {} to compare, and stops after {}.",
        counted(report.history_limits.max_rows, ROW),
        iec_bytes(report.history_limits.max_row_bytes),
        counted(report.max_calls, TOOL_CALL),
        iec_bytes(report.max_analysis_bytes),
        report.recognizer_limits.max_observations,
        format_elapsed(report.max_elapsed_ms.div_ceil(MILLIS_PER_SECOND)),
    )
}

fn iec_bytes(bytes: usize) -> String {
    format_iec_bytes(u64::try_from(bytes).unwrap_or(u64::MAX))
}

fn entry(record: Arc<PermissionRuleRecord>, current: Option<&Path>) -> PermissionEntry {
    PermissionEntry {
        id: Some(record.id.clone()),
        label: row_label(&record.rule.effect, &record_phrase(&record, current)),
        status: Some(record_status(&record, current)),
        source: PickerEntry::Stored(record),
        read_only_policy: false,
        project_config_action: None,
        suggestion: None,
    }
}

/// The tool a stored rule names, as its review recorded it.
fn record_tool(record: &PermissionRuleRecord) -> String {
    if let Some(review) = &record.review {
        return review.tool.clone();
    }
    match &record.rule.subject {
        PermissionSubject::Native { contract, .. } => contract.clone(),
        PermissionSubject::Lua { tool, .. }
        | PermissionSubject::Mcp { tool, .. }
        | PermissionSubject::RemoteWorkcell { tool, .. } => tool.clone(),
        PermissionSubject::RemoteNative { owner, .. } => owner.clone(),
        PermissionSubject::UnknownLegacy { identity } => identity.clone(),
    }
}

/// A stored rule's scope as a row names it. Commands, pages, searches, and
/// paths say what they reach on their own; anything else names its tool. A
/// command names its folder only when it starts outside the rule's project,
/// or outside the current one for a rule bound to none.
fn record_phrase(record: &PermissionRuleRecord, current: Option<&Path>) -> String {
    let phrase = rule_phrase(
        &record.rule,
        record.review.as_ref(),
        record.project.as_deref().or(current),
    );
    let self_describing = !record.rule.resources.is_empty()
        && record.rule.resources.iter().all(|resource| {
            matches!(
                resource.kind,
                PermissionResourceKind::Command
                    | PermissionResourceKind::Url
                    | PermissionResourceKind::Query
                    | PermissionResourceKind::File
                    | PermissionResourceKind::Directory
            )
        });
    match self_describing {
        true => phrase,
        false => format!("{}: {phrase}", record_tool(record)),
    }
}

fn other_project(record: &PermissionRuleRecord, current: Option<&Path>) -> bool {
    current.is_some_and(|current| {
        record
            .project
            .as_deref()
            .is_some_and(|project| project != current)
    })
}

/// Where a stored rule applies, as its row says it.
fn record_status(record: &PermissionRuleRecord, current: Option<&Path>) -> String {
    let place = if record.revoked_at.is_some() {
        REVOKED
    } else if other_project(record, current) {
        OTHER_PROJECT
    } else {
        lifetime_phrase(&record.rule.lifetime)
    };
    row_status(place, YOU)
}

fn effect_word(effect: &StructuredPermissionEffect) -> &'static str {
    match effect {
        StructuredPermissionEffect::Allow => ALLOW,
        StructuredPermissionEffect::Ask => ASK,
        StructuredPermissionEffect::Deny => DENY,
    }
}

fn configured_effect(effect: &Effect) -> StructuredPermissionEffect {
    match effect {
        Effect::Allow => StructuredPermissionEffect::Allow,
        Effect::Ask => StructuredPermissionEffect::Ask,
        Effect::Deny => StructuredPermissionEffect::Deny,
    }
}

/// `Allow  cargo test *`: the effect padded so every scope starts in one
/// column.
fn row_label(effect: &StructuredPermissionEffect, phrase: &str) -> String {
    display_text(
        &format!("{:<EFFECT_WIDTH$}{EFFECT_GAP}{phrase}", effect_word(effect)),
        MAX_DISPLAY_CHARS,
    )
}

fn row_status(place: &str, origin: &str) -> String {
    format!("{place:<PLACE_WIDTH$}{EFFECT_GAP}{origin}")
}

/// A configured rule's scope: `bash: git push *`, `any tool: anything`.
fn config_rule_phrase(tool: &ToolKey, scope: Option<&str>) -> String {
    let tool = match tool {
        ToolKey::Wildcard => ANY_TOOL.into(),
        tool => tool.to_string(),
    };
    format!("{tool}: {}", scope.map_or_else(|| ANYTHING.into(), tilde))
}

/// What the selected row allows, said plainly: the same sentences prompt
/// Details uses, then where it applies and who added it.
fn summary_lines(entry: &PermissionEntry, current: Option<&Path>) -> Vec<Line<'static>> {
    let theme = theme::current();
    let title =
        |text: String| Line::styled(display_text(&text, MAX_DISPLAY_CHARS), theme.panel_title);
    let plain = |text: &str| Line::from(display_text(text, MAX_DISPLAY_CHARS));
    let sentence = |text: &str| {
        Line::from(code_spans_in(
            &display_text(text, MAX_DISPLAY_CHARS),
            Style::default(),
        ))
    };
    let muted =
        |text: String| Line::styled(display_text(&text, MAX_DISPLAY_CHARS), theme.item_desc);
    match &entry.source {
        PickerEntry::Stored(record) => {
            let review = record.review.as_ref();
            let mut lines = vec![title(format!(
                "{} {}",
                effect_word(&record.rule.effect),
                record_phrase(record, current)
            ))];
            lines.extend(
                rule_summary(&record.rule, review, current)
                    .lines
                    .iter()
                    .map(|line| sentence(line)),
            );
            if let Some(input) = review.and_then(|review| review.input.as_ref())
                && let Some(fixed) = fixed_input(&record.rule.arguments, input)
            {
                lines.push(plain(&format!("{FIXED_INPUT}{fixed}")));
            }
            lines.push(Line::default());
            lines.push(muted(if record.revoked_at.is_some() {
                format!("{REVOKED_NOTE}{ADDED_BY_YOU}")
            } else if let Some(project) = record.project.as_deref()
                && other_project(record, current)
            {
                format!(
                    "Remembered for {}{ADDED_BY_YOU}. It doesn't apply in this project.",
                    tilde(&project.to_string_lossy())
                )
            } else {
                format!(
                    "Remembered for {}{ADDED_BY_YOU}",
                    lifetime_phrase(&record.rule.lifetime)
                )
            }));
            if let Some(label) = &record.label {
                lines.push(muted(format!("{NAMED}{label}")));
            }
            lines.push(muted(format!(
                "{TOOL_PREFIX}{}",
                tool_words(
                    &record_tool(record),
                    &record.rule.subject,
                    &record.rule.executor
                )
            )));
            lines
        }
        PickerEntry::Discovered(candidate) => suggestion_lines(candidate),
        PickerEntry::Policy {
            origin,
            rule,
            locator,
        } => vec![
            title(format!(
                "{} {}",
                effect_word(&configured_effect(&rule.effect)),
                config_rule_phrase(&rule.tool, rule.scope.as_deref())
            )),
            plain(match origin {
                RuleOrigin::Builtin => BUILT_IN_RULE,
                RuleOrigin::Plugin => FROM_PLUGIN,
                _ => FROM_CONFIG,
            }),
            Line::default(),
            muted(
                match locator {
                    Some(_) => EDIT_AT_SOURCE,
                    None => READ_ONLY_POLICY,
                }
                .into(),
            ),
        ],
        PickerEntry::Review(candidate) => vec![
            title(format!(
                "{ALLOW} {}",
                config_rule_phrase(
                    candidate.tool.as_ref().unwrap_or(&ToolKey::Wildcard),
                    candidate.scope.as_deref()
                )
            )),
            plain(&format!(
                "An old {} from {}.",
                match candidate.kind {
                    PermissionReviewKind::Rule => "allow rule",
                    PermissionReviewKind::Default => "default allow",
                },
                match candidate.source {
                    PermissionSource::Global => "your global config",
                    PermissionSource::Project => "this project's config",
                    PermissionSource::Conversation => "an earlier conversation",
                }
            )),
            plain(INACTIVE_ALLOW),
        ],
        PickerEntry::ProjectConfig => vec![
            title(PROJECT_CONFIG.into()),
            plain(match entry.project_config_action {
                Some(ProjectConfigAction::RevokeTrust) => TRUSTED_CONFIG,
                _ => UNTRUSTED_CONFIG,
            }),
            Line::default(),
            muted(TRUST_REVIEW.into()),
        ],
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

/// The inputs a rule holds fixed, as `name: value` pairs. A rule fixing only
/// some fields was reviewed as one entry per field, naming it by its path.
fn fixed_input(arguments: &PermissionArgumentConstraint, input: &Value) -> Option<String> {
    let fields = match arguments {
        PermissionArgumentConstraint::Unconstrained => return None,
        PermissionArgumentConstraint::Exact { .. } => return Some(input_description(input, 0)),
        PermissionArgumentConstraint::Selected { .. }
        | PermissionArgumentConstraint::SelectedDigest { .. } => input.as_array()?,
    };
    let pairs: Vec<String> = fields
        .iter()
        .map(|field| {
            let Some(path) = field.get(FIELD_PATH).and_then(Value::as_str) else {
                return input_description(field, 1);
            };
            let name = display_text(path.strip_prefix('/').unwrap_or(path), MAX_FIELD_CHARS);
            match field.get(FIELD_VALUE) {
                Some(value) => format!("{name}: {}", input_description(value, 1)),
                None => format!("{name}: {NOT_SET}"),
            }
        })
        .collect();
    Some(bounded_text(&pairs.join(", "), MAX_DISPLAY_CHARS))
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
    PermissionEntry {
        source: PickerEntry::Review(candidate.clone()),
        id: None,
        label: row_label(
            &StructuredPermissionEffect::Allow,
            &config_rule_phrase(
                candidate.tool.as_ref().unwrap_or(&ToolKey::Wildcard),
                candidate.scope.as_deref(),
            ),
        ),
        status: Some(row_status(
            INACTIVE,
            match candidate.source {
                PermissionSource::Conversation => YOU,
                PermissionSource::Global | PermissionSource::Project => CONFIG,
            },
        )),
        read_only_policy: false,
        project_config_action: None,
        suggestion: None,
    }
}

fn policy_entry(policy: &ActivePolicyRule) -> PermissionEntry {
    PermissionEntry {
        source: PickerEntry::Policy {
            origin: policy.origin,
            rule: policy.rule.clone(),
            locator: policy.verified_local_source_locator.clone(),
        },
        id: None,
        label: row_label(
            &configured_effect(&policy.rule.effect),
            &config_rule_phrase(&policy.rule.tool, policy.rule.scope.as_deref()),
        ),
        status: Some(row_status(ALWAYS, origin_word(policy.origin))),
        read_only_policy: true,
        project_config_action: None,
        suggestion: None,
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
        Hint::bind(key::ENTER, TRUST_HINT),
        Hint::bind(key::TAB, "Details"),
        Hint::bind(key::ESC, "Close"),
    ]
}

fn suggestion_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "View"),
        Hint::bind(DISMISS_SUGGESTION, "Dismiss"),
        Hint::bind(SNOOZE_SUGGESTION, "Snooze 24h"),
    ]
}

fn discovery_footer() -> Vec<Hint> {
    vec![
        Hint::bind(REFRESH_DISCOVERY, "Refresh"),
        Hint::bind(key::TAB, "Details"),
        Hint::bind(key::ESC, "Back"),
    ]
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
        ActivePolicyRule, PermissionArgumentConstraint, PermissionLifetime, PermissionRequest,
        PermissionResourceKind, PermissionResourceSelector, PermissionReviewSource,
        PermissionRuleRecord, RuleOrigin, StructuredPermissionEffect,
        pattern_recognition::{
            CandidateEvidence, InvocationOutcome, ObservationProvenance, PatternCandidate,
            SupportCount, TupleSupport,
        },
        review::{review_for_rule, review_from_candidates},
        selected_input_digest,
    };
    use caudra_config::{
        Effect, PermissionReviewCandidate, PermissionReviewKind, PermissionRule, PermissionSource,
        ToolKey,
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

    use crate::components::command_text::tests::{
        NOT_COLOURED, SHELL_SYNTAX, assert_drawn_in, coloured, syntax_colour,
    };
    use crate::components::permission_prompt::{
        MISSING_SCOPE, THEMES, WIDTHS, assert_plain, buffer_rows,
    };
    use crate::components::{buffer_text, list_picker::PickerItem};
    use crate::{PatternDiscoveryOutcome, test_pattern_discovery_report, theme};

    use super::{
        ALWAYS_ASKS, CONFIRM_REVOKE_MESSAGE, COVERS, DISCOVERY_CANCELLED, DISCOVERY_CLOSING,
        DISCOVERY_EMPTY, DISCOVERY_IDLE, DISCOVERY_LOADING, DISCOVERY_UNVERIFIED, DiscoveryState,
        FIXED_INPUT, INACTIVE, INACTIVE_ALLOW, MAX_DISPLAY_CHARS, MAX_FIELD_CHARS, NOT_ACTIVE,
        NOT_TRUSTED, OMITTED, OTHER_VALUES, PROJECT_CONFIG, PermissionEntry, PermissionsMode,
        PermissionsPicker, PermissionsPickerAction, PickerAction, PickerEntry, ProjectConfigAction,
        REVIEW_SECTION, REVOKE_CAUTION, SCROLLBAR_WIDTH, STILL_ASKS, STORED_SECTION,
        SUGGESTED_SECTION, TRUST_HINT, TRUST_MESSAGE, UNTRUSTED_CONFIG, display_text, entry,
        project_config_entry, suggestion_entry, summary_lines,
    };

    const ROOT: &str = "/project";
    const FILE: &str = "/project/src/main.rs";
    const PATTERN: &str = "**/*.{rs,toml}";
    const COMMAND: &str = "git status --short";
    const COMMAND_PROGRAM: &str = "git";
    const COMMAND_PATTERN: &str = "git status *";
    const SECRET: &str = "top-secret";
    const REDACTED: &str = "[redacted]";
    const CONFIRM_REVOKE: &str = "Revoke this permission?";
    const SUGGESTED_COMMAND: &str = "cargo test <package>";
    const SUGGESTED_STATUS: &str = "4× · 2 conversations";
    const IMPORTED_STATUS: &str = "4× · 2 sessions";
    const SUGGESTED_VALUES_LINE: &str = "<package> is alpha or beta.";
    const SUGGESTED_COVERED: [&str; 2] = ["cargo test alpha", "cargo test beta"];
    const SUGGESTED_COUNT: &str = "2×";
    const SUGGESTED_SEEN: &str = "Seen 4× in 2 conversations, run from /project.";
    const IMPORTED_SEEN: &str =
        "Seen 4× in 2 sessions, run from /project. Caudra cannot verify this history.";
    const SUGGESTED_VALUES: [&str; 2] = ["alpha", "beta"];
    const SUGGESTION_REVISION: u64 = 7;
    const NUMBERED_SLOT: &str = "<pattern";
    const HIDDEN_CONTEXT: [&str; 4] = [
        "Tool identity",
        "Analysis version",
        "path_binding",
        "workcell:shell",
    ];
    const NOT_ACTIVE_WORDS: &str = "not active";
    const UNTRUSTED_NAME: &str = "Trust project config \u{202e} Enter to grant";
    const UNTRUSTED_SOURCE: &str = "\x1b[31mRevoke\ny approve";
    const UNTRUSTED_VALUE: &str = "alpha\x1b[31m\u{202e}";
    const ESCAPED_CONTROLS: [&str; 2] = ["\\u{1b}", "\\u{202e}"];
    const BIDI_OVERRIDE: char = '\u{202e}';
    const UNTRUSTED_WORDS: [&str; 2] = ["Trust project config", "Revoke"];
    const OVERVIEW_SENTENCES: [&str; 5] = [
        "Partial scan.",
        "Read 3 of up to 64 sessions: 48 rows of history (24.0 KiB of 4.0 MiB).",
        "Looked at 12 tool calls (6.0 KiB of commands) and kept 8 to compare.",
        "Found 2 suggestions, 2 shown.",
        "A scan reads at most 64 rows of up to 64.0 KiB each and 64 tool calls with 4.0 MiB of commands",
    ];
    const RAW_BYTE_COUNTS: [&str; 4] = ["24576", "6144", "4194304", "65536"];
    const NARROW_WIDTH: u16 = 40;
    const WIDE_WIDTH: u16 = 140;
    const SHORT_HEIGHT: u16 = 12;
    const LONG_WORD_REPEATS: usize = 24;
    const NAV_LABELS: [&str; 3] = ["short-rule", "long-rule", "last-rule"];
    const LONG_DETAIL: &str = "long constrained argument value ";
    const LONG_DETAIL_REPEATS: usize = 80;
    const MANAGER_TEST_HEIGHT: u16 = 40;
    const DISCOVERY_TEST_ERROR: &str = "History is locked. Refresh to retry.";
    const DISCOVERY_TEST_PARTIAL: &str = "History sample limit reached";
    const DISCOVERY_SAMPLE_COUNT: &str = "Read 2 of up to 64 sessions: 4 rows of history";
    const DISCOVERY_MINIMUM: &str = "at least 2 sessions";
    const EXPORT_DIRECTORY_MODE: u32 = 0o700;
    const EXPORT_ARTIFACT_MODE: u32 = 0o600;
    const EXPORT_LONG_GRANTS: usize = 240;
    const EXPORT_MAX_PAGES: usize = 32;
    const EXPORT_SCAN_BUDGET_MS: u64 = 3_000;
    const EXPORT_ROW_BYTES: usize = 512;
    const EXPORT_LONG_COMMAND: &str = "opsctl release inspect --project warehouse --environment staging --format json --include dependencies --include rollout --include health --target 'deploy/東京/warehouse-green' --config '/project/deployments/production/release coordination/rollout and recovery settings.toml'";
    const EXPORT_UNICODE_PATH: &str =
        "/project/docs/設計/rollout notes/production/recovery checklist.md";
    const EXPORT_ANCHORS: [&str; 2] = ["Allow  git", "Allow  ops"];
    const MODE_TEST_RULE_INDEX: usize = 17;
    const PROPOSAL_ANCHORS: [&str; 2] = ["opsctl", "artifact"];
    const LEGACY_SCOPE: &str = "cargo *";
    const LEGACY_ROW: &str = "Allow  bash: cargo *";
    const LEGACY_ORIGIN: &str = "An old allow rule from this project's config.";
    const POLICY_SCOPE: &str = "git push *";
    const BUILTIN_SCOPE: &str = "git log *";
    const COMMAND_PATTERN_SENTENCE: &str = "Runs `git status` with any arguments";
    const SENTENCE_ROWS: [(&str, &str, &str); 4] = [
        ("Deny   git status --short", "this conversation", "you"),
        ("Ask    bash: git push *", "always", "config"),
        ("Allow  bash: git log *", "always", "built-in"),
        (LEGACY_ROW, "inactive", "config"),
    ];
    const RULE_SUMMARY: [&str; 4] = [
        "Deny git status --short",
        "Runs exactly `git status --short`, started in /project (this project).",
        "Fixed input: command: git status --short",
        "Remembered for this conversation · added by you",
    ];

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

    fn read_details(picker: &mut PermissionsPicker, width: u16, height: u16) -> String {
        picker.detail_focused = true;
        picker.detail.scroll.reset();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
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
                        "bash",
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
                    "bash",
                    json!({"command": COMMAND, "workdir": ROOT}),
                    &[COMMAND],
                ),
                export_rule(
                    "bash",
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
        picker.set_current_project(Some(ROOT.into()));
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
                let mut record = record();
                record.label = (index == 1).then(|| LONG_DETAIL.repeat(LONG_DETAIL_REPEATS));
                let mut permission = entry(Arc::new(record), None);
                permission.id = Some((*label).into());
                permission.label = (*label).into();
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
        assert!(picker.detail.scroll.offset() > 0);
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
        picker.set_discovery(state);
        picker.show_discovery();
        let details = read_details(&mut picker, NARROW_WIDTH, SHORT_HEIGHT);
        for text in [
            expected,
            DISCOVERY_MINIMUM,
            DISCOVERY_EMPTY,
            DISCOVERY_UNVERIFIED,
            DISCOVERY_CLOSING,
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
    }

    #[test_case(ObservationProvenance::Native, SUGGESTED_STATUS, SUGGESTED_SEEN; "native_history")]
    #[test_case(ObservationProvenance::Imported, IMPORTED_STATUS, IMPORTED_SEEN; "imported_history")]
    fn suggestions_say_where_they_were_seen(
        provenance: ObservationProvenance,
        status: &str,
        seen: &str,
    ) {
        let mut candidate = suggestion();
        candidate.evidence.provenance = provenance;
        let mut picker = suggested_picker();
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION, &[candidate]);
        assert_eq!(
            picker
                .picker
                .selected_item()
                .and_then(|entry| entry.status.as_deref()),
            Some(status)
        );
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        let text = read_details(&mut picker, NARROW_WIDTH, SHORT_HEIGHT);
        assert!(text.contains(&compact(seen)), "missing {seen}: {text}");
    }

    #[test_case(24, "value-"; "narrow_ascii")]
    #[test_case(NARROW_WIDTH, "界é/"; "narrow_unicode")]
    #[test_case(25, "界e\u{301}/"; "narrow_combining_graphemes")]
    fn suggestion_pane_wraps_complete_long_templates_and_values(width: u16, word: &str) {
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
        let text = read_details(&mut picker, width, SHORT_HEIGHT);
        let template = format!("cargo {operation} <package>");
        for expected in [template.as_str(), value.as_str()] {
            assert!(
                text.contains(&compact(expected)),
                "missing {expected}: {text}"
            );
        }
        for hidden in [OMITTED, "ObservedTuples", &fingerprint] {
            assert!(!text.contains(&compact(hidden)), "unexpected {hidden}");
        }
        assert!(picker.detail.scroll.offset() > 0);
        assert!(!picker.has_pending_confirmation());
    }

    #[test_case(false, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL); "control_i_in_the_list")]
    #[test_case(true, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL); "control_i_in_the_pane")]
    #[test_case(true, KeyEvent::from(KeyCode::Enter); "enter_in_the_pane")]
    fn suggestion_keys_open_nothing_over_the_pane(pane: bool, key: KeyEvent) {
        let mut picker = suggested_picker();
        if pane {
            picker.handle_key(KeyEvent::from(KeyCode::Enter));
        }
        let before = buffer_text(&export_buffer(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT));
        assert!(matches!(
            picker.handle_key(key),
            PermissionsPickerAction::Consumed
        ));
        assert_eq!(
            buffer_text(&export_buffer(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT)),
            before
        );
        assert_eq!(picker.detail_focused, pane);
        assert!(!picker.has_pending_confirmation());
    }

    #[test]
    fn suggestions_read_as_templates_with_what_they_cover() {
        let mut picker = suggested_picker();
        let rows = buffer_rows(&export_buffer(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT));
        let screen = rows.join("\n");
        assert!(screen.contains(SUGGESTED_SECTION), "{screen}");
        assert!(
            rows.iter()
                .any(|row| row.contains(SUGGESTED_COMMAND) && row.contains(SUGGESTED_STATUS)),
            "missing {SUGGESTED_COMMAND} · {SUGGESTED_STATUS}:\n{screen}"
        );
        let count_columns: Vec<_> = SUGGESTED_COVERED
            .iter()
            .map(|command| {
                rows.iter()
                    .find_map(|row| {
                        row.find(command)?;
                        row.rfind(SUGGESTED_COUNT).map(|at| row[..at].width())
                    })
                    .unwrap_or_else(|| panic!("missing {command}:\n{screen}"))
            })
            .collect();
        assert!(
            count_columns.windows(2).all(|pair| pair[0] == pair[1]),
            "{count_columns:?}"
        );
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PermissionsPickerAction::Consumed
        ));
        let pane = read_details(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT);
        for text in [
            SUGGESTED_COMMAND,
            SUGGESTED_VALUES_LINE,
            COVERS,
            STILL_ASKS,
            OTHER_VALUES,
            SUGGESTED_SEEN,
            NOT_ACTIVE,
        ]
        .into_iter()
        .chain(SUGGESTED_COVERED)
        .chain(ALWAYS_ASKS)
        {
            assert!(pane.contains(&compact(text)), "missing {text}: {pane}");
        }
        for hidden in HIDDEN_CONTEXT
            .into_iter()
            .chain([NUMBERED_SLOT, CONFIRM_REVOKE])
        {
            assert!(
                !pane.contains(&compact(hidden)),
                "unexpected {hidden}: {pane}"
            );
        }
        assert!(picker.pending_revoke.is_none());
        assert!(picker.is_open());
    }

    #[test]
    fn the_discover_screen_says_not_active_once() {
        let mut picker = suggested_picker();
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        let screen = buffer_text(&export_buffer(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT))
            .to_lowercase();
        assert_eq!(screen.matches(NOT_ACTIVE_WORDS).count(), 1, "{screen}");
    }

    #[test]
    fn scan_overview_reads_as_sentences_with_readable_sizes() {
        let mut picker = export_picker("discovery-overview");
        picker.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let overview = read_details(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT);
        for sentence in OVERVIEW_SENTENCES {
            assert!(
                overview.contains(&compact(sentence)),
                "missing {sentence}: {overview}"
            );
        }
        for raw in RAW_BYTE_COUNTS {
            assert!(!overview.contains(raw), "raw byte count {raw}: {overview}");
        }
    }

    #[test_case(PermissionsMode::Rules, STORED_SECTION; "rules")]
    #[test_case(PermissionsMode::Discover, SUGGESTED_SECTION; "discover")]
    fn lists_fill_their_column_from_the_top(mode: PermissionsMode, section: &str) {
        let mut picker = suggested_picker();
        picker.set_mode(mode);
        let rows = buffer_rows(&export_buffer(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT));
        let pane = picker.detail.popup;
        let column = picker.popup.x + 1;
        for row in [pane.y, pane.bottom() - 1] {
            assert!(
                picker.picker.contains(Position::new(column, row)),
                "the list stops short of row {row}"
            );
        }
        let first = &rows[usize::from(pane.y) + 1];
        assert!(first.contains(section), "{first:?}");
    }

    #[test_case('d', false; "dismiss_from_list")]
    #[test_case('s', false; "snooze_from_list")]
    #[test_case('d', true; "dismiss_from_pane")]
    #[test_case('s', true; "snooze_from_pane")]
    fn suggestion_actions_are_explicit_and_bound_to_the_definition(key: char, pane: bool) {
        let mut picker = suggested_picker();
        if pane {
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
    fn untrusted_suggestion_text_is_escaped_hidden_and_never_actions() {
        let mut candidate = suggestion();
        let slot = candidate.definition.slots[0].id;
        candidate.definition.name = UNTRUSTED_NAME.into();
        candidate.evidence.sources = [UNTRUSTED_SOURCE.into()].into();
        candidate.definition.slots[0].domain = ArgumentDomain::ObservedSet {
            values: [UNTRUSTED_VALUE.into()].into(),
        };
        let tuple = BTreeMap::from([(slot, UNTRUSTED_VALUE.to_owned())]);
        candidate.definition.combinations = SlotCombinations::ObservedTuples {
            tuples: [tuple.clone()].into(),
        };
        candidate.evidence.tuples = vec![TupleSupport {
            values: tuple,
            support: candidate.evidence.support.clone(),
        }];
        let rendered = suggestion_entry(Path::new(ROOT), SUGGESTION_REVISION, &candidate).unwrap();
        assert_eq!(rendered.label, SUGGESTED_COMMAND);
        assert_eq!(rendered.section(), Some(SUGGESTED_SECTION));
        let pane = summary_rows(&rendered).join("\n");
        for escaped in ESCAPED_CONTROLS {
            assert!(pane.contains(escaped), "missing {escaped}: {pane}");
        }
        assert!(
            !pane
                .chars()
                .any(|character| character != '\n' && character.is_control()),
            "{pane}"
        );
        assert!(!pane.contains(BIDI_OVERRIDE), "{pane}");
        for hidden in UNTRUSTED_WORDS {
            assert!(!pane.contains(hidden), "unexpected {hidden}: {pane}");
        }
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

    #[test_case(false; "suggestion_removed")]
    #[test_case(true; "context_changed")]
    fn suggestion_refresh_never_revokes_rules(context_changed: bool) {
        let mut picker = suggested_picker();
        let rule_id = picker.entries[0].id.clone();
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        let candidates = if context_changed {
            vec![suggestion()]
        } else {
            Vec::new()
        };
        picker.set_suggestions(Path::new(ROOT), SUGGESTION_REVISION + 1, &candidates);
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

    fn policy(origin: RuleOrigin, effect: Effect, scope: &str) -> ActivePolicyRule {
        ActivePolicyRule {
            origin,
            rule: PermissionRule {
                tool: ToolKey::native("bash"),
                scope: Some(scope.into()),
                effect,
            },
            verified_local_source_locator: None,
        }
    }

    fn legacy_allow() -> PermissionReviewCandidate {
        PermissionReviewCandidate {
            source: PermissionSource::Project,
            kind: PermissionReviewKind::Rule,
            tool: Some(ToolKey::native("bash")),
            scope: Some(LEGACY_SCOPE.into()),
        }
    }

    fn sentence_picker() -> PermissionsPicker {
        let mut picker = PermissionsPicker::new();
        picker.set_current_project(Some(ROOT.into()));
        picker.open(
            vec![record()],
            &[legacy_allow()],
            &[
                policy(RuleOrigin::Config, Effect::Ask, POLICY_SCOPE),
                policy(RuleOrigin::Builtin, Effect::Allow, BUILTIN_SCOPE),
            ],
            false,
            false,
        );
        picker
    }

    #[test]
    fn rule_rows_read_as_sentences() {
        let mut picker = sentence_picker();
        let rows = buffer_rows(&export_buffer(&mut picker, 140, 32));
        for (sentence, place, origin) in SENTENCE_ROWS {
            assert!(
                rows.iter().any(|row| row.contains(sentence)
                    && row.contains(place)
                    && row.contains(origin)),
                "missing {sentence} · {place} · {origin}:\n{}",
                rows.join("\n")
            );
        }
        let summary = read_details(&mut picker, 140, 32);
        for text in RULE_SUMMARY {
            assert!(
                summary.contains(&compact(text)),
                "missing {text}: {summary}"
            );
        }
    }

    /// Every manager screen a person reads: each row with and without its
    /// detail, the revoke and trust confirmations, a suggestion with its pane,
    /// and the scan overview.
    fn manager_surfaces(width: u16) -> Vec<(&'static str, Vec<String>)> {
        let mut surfaces = Vec::new();
        let mut capture = |surface, picker: &mut PermissionsPicker| {
            surfaces.push((
                surface,
                buffer_rows(&export_buffer(picker, width, MANAGER_TEST_HEIGHT)),
            ));
        };
        let mut picker = sentence_picker();
        for index in 0..picker.visible_entries().len() {
            picker.picker.select(index);
            capture("list", &mut picker);
            picker.detail_focused = true;
            capture("detail", &mut picker);
            picker.detail_focused = false;
        }
        picker.picker.select(0);
        picker.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        capture("revoke", &mut picker);
        let mut trust = PermissionsPicker::new();
        trust.open(Vec::new(), &[], &[], true, false);
        trust.handle_key(KeyEvent::from(KeyCode::Enter));
        capture("trust", &mut trust);
        let mut discover = suggested_picker();
        capture("suggestion", &mut discover);
        discover.handle_key(KeyEvent::from(KeyCode::Enter));
        capture("suggestion pane", &mut discover);
        let mut overview = export_picker("discovery-overview");
        overview.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        capture("overview", &mut overview);
        surfaces
    }

    #[test]
    fn colours_leave_every_manager_surface_word_for_word() {
        let plain = manager_surfaces(WIDE_WIDTH);
        coloured();
        assert_eq!(manager_surfaces(WIDE_WIDTH), plain);
    }

    /// The rule's row is left unselected, since a selected row is drawn in
    /// the selection's colours.
    #[test]
    fn stored_commands_are_drawn_in_shell_colours() {
        coloured();
        let shell = syntax_colour(SHELL_SYNTAX, COMMAND, COMMAND_PROGRAM);
        assert!(shell.is_some(), "{NOT_COLOURED}");
        let mut picker = sentence_picker();
        let entries = picker.visible_entries();
        let stored = entries
            .iter()
            .position(|entry| matches!(entry.source, PickerEntry::Stored(_)))
            .unwrap();
        picker.picker.select((stored + 1) % entries.len());
        let buffer = export_buffer(&mut picker, WIDE_WIDTH, MANAGER_TEST_HEIGHT);
        assert_drawn_in(&buffer, COMMAND, 0, shell);
    }

    #[test]
    fn manager_surfaces_never_show_internal_terms() {
        for name in THEMES {
            theme::set(theme::load_by_name(name).unwrap());
            for width in WIDTHS {
                for (surface, rows) in manager_surfaces(width) {
                    assert_plain(&rows, &format!("{name} {width} {surface}"));
                }
            }
        }
    }

    #[test]
    fn revocation_needs_a_plain_confirmation() {
        let record = record();
        let id = record.id.clone();
        let PermissionArgumentConstraint::Exact { digest } = &record.rule.arguments else {
            panic!("expected exact input")
        };
        let digest = digest.clone();
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        let screen = compact(&buffer_text(&export_buffer(&mut picker, 100, 24)))
            + read_details(&mut picker, 100, 24).as_str();
        assert!(screen.contains(&compact(COMMAND)));
        assert!(!screen.contains(&digest[..10]));
        assert!(!screen.contains(&id[..10]));

        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            PermissionsPickerAction::Consumed
        ));
        let screen = read_details(&mut picker, 100, 24);
        for text in [CONFIRM_REVOKE_MESSAGE, REVOKE_CAUTION] {
            assert!(screen.contains(&compact(text)), "missing {text}: {screen}");
        }
        assert!(matches!(
            picker.handle_key(KeyEvent::from(KeyCode::Enter)),
            PermissionsPickerAction::Revoke(selected) if selected == id
        ));
    }

    #[test]
    fn shows_inactive_legacy_allows_for_review() {
        let mut picker = PermissionsPicker::new();
        picker.open(Vec::new(), &[legacy_allow()], &[], false, false);
        let screen = compact(&buffer_text(&export_buffer(&mut picker, 100, 16)))
            + read_details(&mut picker, 100, 16).as_str();
        for text in [
            REVIEW_SECTION,
            LEGACY_ROW,
            INACTIVE,
            LEGACY_ORIGIN,
            INACTIVE_ALLOW,
        ] {
            assert!(screen.contains(&compact(text)), "missing {text}: {screen}");
        }
    }

    #[test]
    fn project_config_trust_requires_confirmation_and_can_be_cancelled() {
        let mut picker = PermissionsPicker::new();
        picker.open(Vec::new(), &[], &[], true, false);
        let screen = compact(&buffer_text(&export_buffer(&mut picker, 120, 18)))
            + read_details(&mut picker, 120, 18).as_str();
        for text in [PROJECT_CONFIG, UNTRUSTED_CONFIG, NOT_TRUSTED, TRUST_HINT] {
            assert!(screen.contains(&compact(text)), "missing {text}: {screen}");
        }

        let enter = KeyEvent::from(KeyCode::Enter);
        assert!(matches!(
            picker.handle_key(enter),
            PermissionsPickerAction::Consumed
        ));
        let screen = read_details(&mut picker, 120, 18);
        assert!(screen.contains(&compact(TRUST_MESSAGE)), "{screen}");

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
        let screen = compact(&buffer_text(&export_buffer(&mut picker, 70, 16)))
            + read_details(&mut picker, 70, 16).as_str();
        for text in [COMMAND_PATTERN, COMMAND_PATTERN_SENTENCE] {
            assert!(screen.contains(&compact(text)), "missing {text}: {screen}");
        }
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
        assert!(screen.contains(&compact(FIXED_INPUT)), "{screen}");
        assert!(screen.contains(&compact(expected)), "{screen}");
        for key in input.as_object().unwrap().keys() {
            assert!(screen.contains(&format!("{key}:")), "{screen}");
        }
    }

    fn summary_rows(entry: &PermissionEntry) -> Vec<String> {
        summary_lines(entry, None)
            .iter()
            .map(ToString::to_string)
            .collect()
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
        let rendered = entry(Arc::new(record), None);
        let mut rows = summary_rows(&rendered);
        rows.push(rendered.label.clone());
        assert!(rows.iter().any(|row| row == MISSING_SCOPE), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains(&id[..10])));
        assert_plain(&rows, "missing review");
        assert_eq!(rendered.id.as_deref(), Some(id.as_str()));
    }

    #[test_case("file_read", json!({"filePath": FILE, "token": SECRET}); "secret_field")]
    #[test_case("bash", json!({"command": format!("deploy --token {SECRET} --target production")}); "command_token")]
    fn builder_redaction_survives_picker_rendering(tool: &str, input: Value) {
        let resource = input.get("command").and_then(Value::as_str).unwrap_or(FILE);
        let record = reviewed_record(tool, input.clone(), &[resource]);
        let rendered = entry(Arc::new(record), None);
        let mut rows = summary_rows(&rendered);
        rows.push(rendered.label.clone());
        assert!(rows.iter().any(|row| row.contains(REDACTED)), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains(SECRET)), "{rows:?}");
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
        let rendered = entry(Arc::new(record), None);
        let fixed: Vec<String> = summary_rows(&rendered)
            .into_iter()
            .filter(|row| row.starts_with(FIXED_INPUT))
            .collect();
        if broad {
            assert!(fixed.is_empty(), "{fixed:?}");
        } else {
            assert_eq!(fixed, [format!("{FIXED_INPUT}pattern: {PATTERN}")]);
        }
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
        review.input = Some(json!({text.clone(): text.clone()}));
        let shown = &mut review.resources[0];
        shown.value = shown
            .value
            .as_ref()
            .map(|value| value.replace(COMMAND, &text));
        record.rule.resources[0].kind = PermissionResourceKind::Custom {
            name: controls.into(),
        };
        let rendered = entry(Arc::new(record), None);
        let mut rows = summary_rows(&rendered);
        rows.push(rendered.label.clone());
        assert!(rows.iter().any(|row| row.contains('界')), "{rows:?}");
        for row in rows {
            assert!(!row.chars().any(char::is_control), "{row:?}");
            assert!(row.chars().count() <= MAX_DISPLAY_CHARS + OMITTED.len());
        }
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
        let selected = entry(Arc::new(record.clone()), None);
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
    fn wheel_scrolls_the_selected_summary(width: u16) {
        let mut record = record();
        let shown = &mut record.review.as_mut().unwrap().resources[0];
        shown.value = shown
            .value
            .as_ref()
            .map(|value| value.replace(COMMAND, &COMMAND.repeat(EXPORT_LONG_GRANTS)));
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        picker.handle_key(KeyEvent::from(KeyCode::Enter));
        export_buffer(&mut picker, width, MANAGER_TEST_HEIGHT);
        let selected = picker.picker.selected_index();
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: picker.detail.popup.x,
            row: picker.detail.popup.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(picker.detail.scroll.offset(), 1);
        assert_eq!(picker.picker.selected_index(), selected);
    }
}
