use caudra_agent::permissions::{
    EffectivePermissionRule, PermissionArgumentConstraint, PermissionLifetime,
    PermissionResourceSelector, PermissionRuleRecord, PermissionSubject,
    StructuredPermissionEffect,
};
use caudra_config::{
    Effect, PermissionReviewCandidate, PermissionReviewKind, PermissionRule, PermissionSource,
};
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
    RemoveLegacy(PermissionRule),
}

#[derive(Clone)]
struct PermissionEntry {
    id: Option<String>,
    tool: String,
    detail: String,
    legacy_rule: Option<PermissionRule>,
    read_only: bool,
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
    pending_revoke: Option<String>,
    pending_legacy_removal: Option<PermissionRule>,
}

impl PermissionsPicker {
    pub(crate) fn new() -> Self {
        let mut picker = ListPicker::new().with_width_percent(80);
        picker.set_empty_text(EMPTY);
        picker.set_footer_builder(footer);
        Self {
            picker,
            pending_revoke: None,
            pending_legacy_removal: None,
        }
    }

    pub(crate) fn open(
        &mut self,
        rules: Vec<PermissionRuleRecord>,
        review_candidates: &[PermissionReviewCandidate],
        effective_policy: &[EffectivePermissionRule],
    ) {
        let entries = rules
            .into_iter()
            .map(entry)
            .chain(review_candidates.iter().map(review_entry))
            .chain(effective_policy.iter().map(policy_entry))
            .collect();
        self.pending_revoke = None;
        self.pending_legacy_removal = None;
        self.picker.set_info_text(None);
        self.picker.open(entries, TITLE);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PermissionsPickerAction {
        if let Some(rule) = self.pending_legacy_removal.clone() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.pending_legacy_removal = None;
                    PermissionsPickerAction::RemoveLegacy(rule)
                }
                KeyCode::Esc => {
                    self.pending_legacy_removal = None;
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
                if let Some(id) = entry.id.clone() {
                    self.confirm_revoke(id);
                } else if let Some(rule) = entry.legacy_rule.clone()
                    && !entry.read_only
                {
                    self.confirm_legacy_removal(rule);
                } else {
                    let has_legacy_rule = entry.legacy_rule.is_some();
                    self.show_read_only(has_legacy_rule);
                }
            }
            return PermissionsPickerAction::Consumed;
        }
        let action = self.picker.handle_key(key);
        self.map_action(action)
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> PermissionsPickerAction {
        let action = self.picker.handle_mouse(event);
        self.map_action(action)
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
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
                if let Some(id) = entry.id {
                    self.confirm_revoke(id);
                } else if let Some(rule) = entry.legacy_rule
                    && !entry.read_only
                {
                    self.confirm_legacy_removal(rule);
                } else {
                    self.picker.set_info_text(Some(
                        "This policy is read-only here. Edit its configuration or plugin source to change it.".into(),
                    ));
                }
                PermissionsPickerAction::Consumed
            }
            PickerAction::Close => PermissionsPickerAction::Close,
        }
    }

    fn confirm_revoke(&mut self, id: String) {
        self.pending_revoke = Some(id);
        self.picker.set_info_text(Some(
            "Revoke this permission? Press Enter/y to confirm or Esc to cancel.".into(),
        ));
    }

    fn confirm_legacy_removal(&mut self, rule: PermissionRule) {
        self.pending_legacy_removal = Some(rule);
        self.picker.set_info_text(Some(
            "Remove this legacy conversation rule? Press Enter/y to confirm or Esc to cancel."
                .into(),
        ));
    }

    fn show_read_only(&mut self, has_legacy_rule: bool) {
        let message = if has_legacy_rule {
            "This policy is read-only here. Edit its configuration or plugin source to change it."
        } else {
            "This legacy allow is inactive. Re-approve the next exact request or remove the old config entry."
        };
        self.picker.set_info_text(Some(message.into()));
    }
}

impl Overlay for PermissionsPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.pending_revoke = None;
        self.pending_legacy_removal = None;
        self.picker.close();
    }
}

fn entry(record: PermissionRuleRecord) -> PermissionEntry {
    let tool = match &record.rule.subject {
        PermissionSubject::Native { contract, .. } => contract.clone(),
        PermissionSubject::Lua { plugin, tool, .. } => format!("{plugin}:{tool}"),
        PermissionSubject::Mcp { server, tool, .. } => format!("{server}.{tool}"),
        PermissionSubject::UnknownLegacy { identity } => identity.clone(),
    };
    let mut tool = tool;
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
        "[{}] {} · {}{}",
        authority_badge(&record),
        effect_name(&record.rule.effect),
        lifetime_name(&record.rule.lifetime),
        id,
    );
    PermissionEntry {
        id: Some(record.id),
        tool: escape_terminal_controls(&tool),
        detail,
        legacy_rule: None,
        read_only: false,
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
        PermissionReviewKind::AllowAll => "allow all",
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
        legacy_rule: (candidate.source == PermissionSource::Conversation).then(|| PermissionRule {
            tool: candidate
                .tool
                .clone()
                .unwrap_or(caudra_config::ToolKey::Wildcard),
            scope: candidate.scope.clone(),
            effect: Effect::Allow,
        }),
        read_only: candidate.source != PermissionSource::Conversation,
    }
}

fn policy_entry(entry: &EffectivePermissionRule) -> PermissionEntry {
    let scope = entry.rule.scope.as_deref().unwrap_or("<all>");
    PermissionEntry {
        id: None,
        tool: escape_terminal_controls(&entry.rule.tool.to_string()),
        detail: format!(
            "[legacy] {} · {} · scope {}{}",
            match entry.rule.effect {
                Effect::Allow => "allow",
                Effect::Deny => "deny",
            },
            entry.source,
            escape_terminal_controls(scope),
            if entry.removable {
                " · removable"
            } else {
                " · read-only"
            },
        ),
        legacy_rule: Some(entry.rule.clone()),
        read_only: !entry.removable,
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
        picker.open(vec![record], &[], &[]);
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
}
