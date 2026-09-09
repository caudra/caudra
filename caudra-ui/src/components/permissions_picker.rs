use caudra_agent::permissions::{
    ActivePolicyRule, PermissionArgumentConstraint, PermissionLifetime, PermissionResourceSelector,
    PermissionRuleRecord, PermissionSubject, StructuredPermissionEffect,
};
use caudra_config::{Effect, PermissionReviewCandidate, PermissionReviewKind, PermissionSource};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{Overlay, escape_terminal_controls, hint_line};

const TITLE: &str = " Active Structured Permissions ";
const EMPTY: &str = "No active structured permission rules";

pub(crate) enum PermissionsPickerAction {
    Consumed,
    Close,
    Revoke(String),
    TrustProjectConfig,
    RevokeProjectConfigTrust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectConfigAction {
    Trust,
    RevokeTrust,
}

#[derive(Clone, PartialEq, Eq)]
struct PermissionEntry {
    id: Option<String>,
    tool: String,
    detail: String,
    read_only_policy: bool,
    project_config_action: Option<ProjectConfigAction>,
}

impl PickerItem for PermissionEntry {
    fn label(&self) -> &str {
        &self.tool
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }
}

pub(crate) struct PermissionsPicker {
    picker: ListPicker<PermissionEntry>,
    entries: Vec<PermissionEntry>,
    pending_revoke: Option<String>,
    pending_project_config_action: Option<ProjectConfigAction>,
}

impl PermissionsPicker {
    pub(crate) fn new() -> Self {
        let mut picker = ListPicker::new().with_width_percent(80);
        picker.set_empty_text(EMPTY);
        picker.set_footer_builder(footer);
        Self {
            picker,
            entries: Vec::new(),
            pending_revoke: None,
            pending_project_config_action: None,
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
        self.picker.set_info_text(None);
        self.picker
            .set_footer_builder(if needs_project_config_trust || project_config_trusted {
                trust_footer
            } else {
                footer
            });
        self.picker.open(self.entries.clone(), TITLE);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PermissionsPickerAction {
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
                    self.picker.set_info_text(None);
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
                    self.picker.set_info_text(None);
                    PermissionsPickerAction::Consumed
                }
                _ => PermissionsPickerAction::Consumed,
            };
        }
        if key.code == KeyCode::Enter {
            if let Some(entry) = self.picker.selected_item() {
                if let Some(action) = entry.project_config_action {
                    self.confirm_project_config_action(action);
                } else if let Some(id) = entry.id.clone() {
                    self.confirm_revoke(id);
                } else {
                    self.show_read_only(entry.read_only_policy);
                }
            }
            return PermissionsPickerAction::Consumed;
        }
        let action = self.picker.handle_key(key);
        self.map_action(action)
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> PermissionsPickerAction {
        if self.has_pending_confirmation() {
            return PermissionsPickerAction::Consumed;
        }
        let action = self.picker.handle_mouse(event);
        self.map_action(action)
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        if self.has_pending_confirmation() {
            return true;
        }
        self.picker.handle_paste(text)
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        if self.has_pending_confirmation() {
            return;
        }
        self.picker.scroll(delta);
    }

    pub(crate) fn contains(&self, position: ratatui::layout::Position) -> bool {
        self.picker.contains(position)
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    fn map_action(&mut self, action: PickerAction<PermissionEntry>) -> PermissionsPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => PermissionsPickerAction::Consumed,
            PickerAction::Select(entry) => {
                self.picker.open(self.entries.clone(), TITLE);
                self.picker.select_item_by(|candidate| candidate == &entry);
                if let Some(action) = entry.project_config_action {
                    self.confirm_project_config_action(action);
                } else if let Some(id) = entry.id {
                    self.confirm_revoke(id);
                } else {
                    self.show_read_only(entry.read_only_policy);
                }
                PermissionsPickerAction::Consumed
            }
            PickerAction::Close => PermissionsPickerAction::Close,
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
        self.picker.set_info_text(Some(message.into()));
    }

    fn confirm_revoke(&mut self, id: String) {
        self.pending_revoke = Some(id);
        self.picker.set_info_text(Some(
            "Revoke this permission? Press Enter/y to confirm or Esc to cancel.".into(),
        ));
    }

    fn show_read_only(&mut self, read_only_policy: bool) {
        let message = if read_only_policy {
            "This policy is read-only here. Edit its configuration or plugin source to change it."
        } else {
            "This legacy allow is inactive. Re-approve the next exact request or remove the old config entry."
        };
        self.picker.set_info_text(Some(message.into()));
    }

    fn has_pending_confirmation(&self) -> bool {
        self.pending_revoke.is_some() || self.pending_project_config_action.is_some()
    }
}

impl Overlay for PermissionsPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.pending_revoke = None;
        self.pending_project_config_action = None;
        self.picker.close();
    }
}

fn project_config_entry(action: ProjectConfigAction) -> PermissionEntry {
    let trusted = action == ProjectConfigAction::RevokeTrust;
    PermissionEntry {
        id: None,
        tool: "Project permissions.toml".into(),
        detail: if trusted {
            "shell allow patterns are active · trust can be revoked"
        } else {
            "shell allow patterns are inactive · no authority has been granted"
        }
        .into(),
        read_only_policy: false,
        project_config_action: Some(action),
    }
}

fn entry(record: PermissionRuleRecord) -> PermissionEntry {
    let tool = match &record.rule.subject {
        PermissionSubject::Native { contract, .. } => contract.clone(),
        PermissionSubject::Lua { plugin, tool, .. } => format!("{plugin}:{tool}"),
        PermissionSubject::Mcp { server, tool, .. } => format!("{server}.{tool}"),
        PermissionSubject::UnknownLegacy { identity } => identity.clone(),
    };
    let command_patterns: Vec<_> = record
        .rule
        .resources
        .iter()
        .filter_map(|resource| match &resource.selector {
            PermissionResourceSelector::CommandPattern { pattern } => Some(pattern.as_str()),
            _ => None,
        })
        .collect();
    let mut tool = tool;
    let pattern_detail = if command_patterns.is_empty() {
        String::new()
    } else {
        format!(
            "patterns {} · ",
            escape_terminal_controls(&command_patterns.join(", "))
        )
    };
    if let PermissionArgumentConstraint::Exact { digest } = &record.rule.arguments {
        tool.push_str(&format!(" · input {}", &digest[..digest.len().min(10)]));
    }
    if let Some(review) = record
        .review
        .as_ref()
        .and_then(|review| serde_json::to_string(review).ok())
    {
        tool.push_str(&format!(" · args {}", escape_terminal_controls(&review)));
    }
    let id = match &record.rule.arguments {
        PermissionArgumentConstraint::Exact { .. } => String::new(),
        _ => format!(" · id {}", &record.id[..record.id.len().min(10)]),
    };
    let detail = format!(
        "{pattern_detail}[{}] {} · {}{}",
        authority_badge(&record),
        effect_name(&record.rule.effect),
        lifetime_name(&record.rule.lifetime),
        id,
    );
    PermissionEntry {
        id: Some(record.id),
        tool: escape_terminal_controls(&tool),
        detail,
        read_only_policy: false,
        project_config_action: None,
    }
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
        id: None,
        tool: escape_terminal_controls(&tool),
        detail: format!(
            "[needs review] inactive {kind} · {source} · scope {}",
            escape_terminal_controls(scope)
        ),
        read_only_policy: false,
        project_config_action: None,
    }
}

fn policy_entry(entry: &ActivePolicyRule) -> PermissionEntry {
    let scope = entry.rule.scope.as_deref().unwrap_or("<all>");
    PermissionEntry {
        id: None,
        tool: escape_terminal_controls(&entry.rule.tool.to_string()),
        detail: format!(
            "[policy] {} · {} · scope {} · read-only",
            match entry.rule.effect {
                Effect::Allow => "allow",
                Effect::Ask => "ask",
                Effect::Deny => "deny",
            },
            entry.source,
            escape_terminal_controls(scope),
        ),
        read_only_policy: true,
        project_config_action: None,
    }
}

fn authority_badge(record: &PermissionRuleRecord) -> String {
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

fn footer() -> Line<'static> {
    hint_line(&[("Enter", "Revoke"), ("Esc", "Close")])
}

fn trust_footer() -> Line<'static> {
    hint_line(&[("Enter", "Trust or revoke"), ("Esc", "Close")])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use caudra_agent::permissions::{
        PermissionExecutorKind, PermissionResourceConstraint, PermissionResourceKind,
        StructuredPermissionRule,
    };
    use caudra_config::{
        PermissionReviewCandidate, PermissionReviewKind, PermissionSource, ToolKey,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::components::buffer_text;

    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn record() -> PermissionRuleRecord {
        PermissionRuleRecord::conversation_with_review(
            StructuredPermissionRule {
                subject: PermissionSubject::Native {
                    owner: "caudra".into(),
                    contract: "bash".into(),
                },
                executor: PermissionExecutorKind::Native,
                resources: vec![PermissionResourceConstraint {
                    kind: PermissionResourceKind::Command,
                    selector: PermissionResourceSelector::Digest {
                        digest: DIGEST.into(),
                    },
                    access: None,
                    protected: Some(false),
                    attributes: BTreeMap::new(),
                }],
                arguments: PermissionArgumentConstraint::Exact {
                    digest: DIGEST.into(),
                },
                lifetime: PermissionLifetime::Conversation,
                effect: StructuredPermissionEffect::Deny,
                family: None,
            },
            Some(serde_json::json!({"<field:1>": "<string:10 chars>"})),
        )
        .unwrap()
    }

    #[test]
    fn shows_structured_scope_and_requires_confirmed_revocation() {
        let record = record();
        let id = record.id.clone();
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains("bash"));
        assert!(screen.contains("[exact:exact-resource] deny"));
        assert!(screen.contains("conversation"));
        assert!(screen.contains("args"));
        assert!(screen.contains(&DIGEST[..10]));

        let enter = KeyEvent::from(KeyCode::Enter);
        assert!(matches!(
            picker.handle_key(enter),
            PermissionsPickerAction::Consumed
        ));
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
        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains("needs review"));
        assert!(screen.contains("inactive allow rule"));
        assert!(screen.contains("project config"));
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
        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains("Project permissions.toml"));
        assert!(screen.contains("shell allow patterns are inactive"));
        assert!(screen.contains("no authority has been granted"));
        assert!(screen.contains("Trust or revoke"));

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
        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains("Trust the exact project shell allow configuration"));
        assert!(screen.contains("Edits invalidate trust"));

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
            pattern: "git status *".into(),
        };
        let mut picker = PermissionsPicker::new();
        picker.open(vec![record], &[], &[], false, false);
        let backend = TestBackend::new(70, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();

        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains("patterns git status *"));
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
            PermissionsPickerAction::Revoke(id) if id == selected_id
        ));
    }
}
