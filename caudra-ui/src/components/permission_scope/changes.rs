use caudra_agent::permissions::editor::{
    EditField, NormalizedPermissionDraft, SelectorValue, SemanticChange, VerifiedValue,
};
use caudra_agent::permissions::{
    argument_constraint_matches, canonical_json_sha256, selected_input_pointer,
};
use caudra_grab::grab_scope;
use caudra_storage::permission_patterns::{
    ArgumentDomain, ArgumentRole, OptionLikePolicy, PatternDefinition, PatternToken,
    SlotCombinations,
};
use caudra_storage::permission_state::{
    PermissionArgumentConstraint, PermissionResourceConstraint, PermissionResourceKind,
    PermissionResourceSelector, PermissionSubject, RemotePermissionIdentity,
    StructuredPermissionRule,
};
use caudra_workspace::AuthorityIdentity;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use unicode_width::UnicodeWidthStr;

use super::model::{
    access_name, effect_name, lifetime_name, literal, resource_kind, selector_mode,
};
use crate::components::ModalScroll;
use crate::components::permission_prompt::{likely_secret_key, sensitive_text};
use crate::theme::Theme;

pub(super) const DETAIL_ROWS: u16 = 5;
pub(super) const COMPACT_DETAIL_ROWS: u16 = 4;
const ABSENT: &str = "Absent";
const OPAQUE: &str = "Opaque identity · no verified preimage";
const INDEPENDENT: &str = "INDEPENDENT · any combination of allowed slot values";
const TUPLES_ONLY: &str = "ALLOWED TUPLES ONLY · no other combinations";
const UNCHANGED_SECRET: &str = "[redacted · unchanged]";
const BEFORE_SECRET: &str = "[redacted · prior value]";
const AFTER_SECRET: &str = "[redacted · replacement value]";
const PAGE_ROWS: i32 = 4;
const BUTTON_WIDTH: u16 = 3;
const BUTTON_GAP: u16 = 1;
const BUTTONS_WIDTH: u16 = (BUTTON_WIDTH + BUTTON_GAP) * 2;
const INPUT_VALUES: &str = "Arguments · Input";
const SELECTED_VALUE: &str = "Argument ";
const VERIFIED_INPUT: &str = "Host-verified input";
const MISSING_POINTER: &str = "Missing pointer · not JSON null";
const REDACTED_VALUE: &str = "Redacted value · SHA-256";

#[derive(PartialEq, Eq)]
enum FactValue {
    Visible(String),
    Hidden(String),
}

impl FactValue {
    fn display(&self, changed: bool, after: bool) -> String {
        match self {
            Self::Visible(value) => value.clone(),
            Self::Hidden(_) => if !changed {
                UNCHANGED_SECRET
            } else if after {
                AFTER_SECRET
            } else {
                BEFORE_SECRET
            }
            .into(),
        }
    }
}

#[derive(Default)]
struct Facts(BTreeMap<String, FactValue>);

impl Facts {
    fn add(&mut self, label: impl Into<String>, value: impl Into<String>) {
        self.0
            .insert(label.into(), FactValue::Visible(value.into()));
    }

    fn literal(&mut self, label: impl Into<String>, value: &str, hidden: bool) {
        self.0.insert(
            label.into(),
            if hidden {
                FactValue::Hidden(value.into())
            } else {
                FactValue::Visible(literal(value))
            },
        );
    }

    fn authority(&mut self, prefix: &str, identity: &AuthorityIdentity) {
        for (label, value) in [
            ("Trust anchor", identity.trust_anchor().as_str()),
            ("Server", identity.server_id()),
            ("Workspace", identity.workspace_id()),
            ("Generation", identity.workspace_generation()),
            ("Namespace", identity.resource_namespace_version()),
        ] {
            self.literal(format!("{prefix} · {label}"), value, false);
        }
    }

    fn remote(&mut self, prefix: &str, identity: &RemotePermissionIdentity) {
        self.authority(&format!("{prefix} · Authority"), &identity.authority);
        self.authority(
            &format!("{prefix} · Principal authority"),
            identity.principal.authority(),
        );
        self.authority(
            &format!("{prefix} · Project authority"),
            identity.project.authority(),
        );
        self.literal(
            format!("{prefix} · Principal"),
            identity.principal.subject(),
            false,
        );
        self.literal(
            format!("{prefix} · Project key"),
            identity.project.key().as_str(),
            false,
        );
    }

    fn identity(&mut self, rule: &StructuredPermissionRule) {
        self.add("Executor", format!("{:?}", rule.executor));
        self.add(
            "Capability family",
            rule.family
                .as_ref()
                .map_or_else(|| "Single tool only".into(), |family| format!("{family:?}")),
        );
        let kind = match &rule.subject {
            PermissionSubject::Native { owner, contract } => {
                self.literal("Owner", owner, false);
                self.literal("Contract", contract, false);
                "Native"
            }
            PermissionSubject::Lua {
                plugin,
                tool,
                contract,
            } => {
                self.literal("Plugin", plugin, false);
                self.literal("Tool", tool, false);
                self.literal("Contract", contract, false);
                "Lua"
            }
            PermissionSubject::Mcp {
                server,
                authority,
                tool,
                contract,
            } => {
                self.literal("Server", server, false);
                self.literal("Authority", authority, false);
                self.literal("Tool", tool, false);
                self.literal("Contract", contract, false);
                "MCP"
            }
            PermissionSubject::RemoteWorkcell {
                identity,
                tool,
                contract,
            } => {
                self.remote("Binding", identity);
                self.literal("Tool", tool, false);
                self.literal("Contract", contract, false);
                "Remote Workcell"
            }
            PermissionSubject::RemoteNative {
                identity,
                owner,
                contract,
            } => {
                self.remote("Binding", identity);
                self.literal("Owner", owner, false);
                self.literal("Contract", contract, false);
                "Remote native"
            }
            PermissionSubject::UnknownLegacy { identity } => {
                self.literal("Legacy identity", identity, false);
                "Unverified legacy"
            }
        };
        self.add("Subject", kind);
    }

    fn resource(
        &mut self,
        index: usize,
        resource: &PermissionResourceConstraint,
        normalized: Option<&NormalizedPermissionDraft>,
    ) {
        let prefix = format!("Target {}", index + 1);
        self.add(format!("{prefix} · Kind"), resource_kind(&resource.kind));
        self.add(
            format!("{prefix} · Kind class"),
            match resource.kind {
                PermissionResourceKind::File => "File",
                PermissionResourceKind::Directory => "Directory",
                PermissionResourceKind::Url => "URL",
                PermissionResourceKind::Command => "Command",
                PermissionResourceKind::Query => "Query",
                PermissionResourceKind::RemoteFile { .. } => "Remote file",
                PermissionResourceKind::RemoteDirectory { .. } => "Remote directory",
                PermissionResourceKind::RemoteResource { .. } => "Remote custom resource",
                PermissionResourceKind::Custom { .. } => "Custom",
            },
        );
        match &resource.kind {
            PermissionResourceKind::RemoteFile { identity }
            | PermissionResourceKind::RemoteDirectory { identity }
            | PermissionResourceKind::RemoteResource { identity, .. } => {
                self.remote(&format!("{prefix} · Kind binding"), identity)
            }
            _ => {}
        }
        self.add(
            format!("{prefix} · Access"),
            access_name(resource.access.as_ref()),
        );
        self.add(
            format!("{prefix} · Protection"),
            match resource.protected {
                Some(true) => "Protected only",
                Some(false) => "Unprotected only",
                None => "ANY protection",
            },
        );
        self.selector(
            &prefix,
            &resource.selector,
            verified_selector(normalized, &EditField::Resource(index), &resource.selector),
        );
        self.add(
            format!("{prefix} · Attributes"),
            if resource.attributes.is_empty() {
                "No attribute predicates"
            } else {
                "ALL OF these attribute predicates"
            },
        );
        for (name, selector) in &resource.attributes {
            self.selector(
                &format!("{prefix} · Attribute {}", literal(name)),
                selector,
                verified_selector(
                    normalized,
                    &EditField::Attribute {
                        resource: index,
                        name: name.clone(),
                    },
                    selector,
                ),
            );
        }
    }

    fn selector(
        &mut self,
        prefix: &str,
        selector: &PermissionResourceSelector,
        verified: Option<&SelectorValue>,
    ) {
        self.add(format!("{prefix} · Match"), selector_mode(selector));
        match selector {
            PermissionResourceSelector::Exact { value }
            | PermissionResourceSelector::Prefix { value } => {
                self.literal(format!("{prefix} · Value"), value, false)
            }
            PermissionResourceSelector::Subtree { root } => {
                self.literal(format!("{prefix} · Root"), root, false)
            }
            PermissionResourceSelector::CommandPattern { pattern } => {
                self.literal(format!("{prefix} · Token prefix"), pattern, false)
            }
            PermissionResourceSelector::CommandTemplate { definition } => {
                self.pattern(prefix, definition)
            }
            PermissionResourceSelector::Digest { digest }
            | PermissionResourceSelector::FilesystemSubtreeDigest { digest }
            | PermissionResourceSelector::UrlSubtreeDigest { digest }
            | PermissionResourceSelector::UrlOriginDigest { digest } => {
                self.add(format!("{prefix} · SHA-256"), digest.clone());
                self.add(
                    format!("{prefix} · Preimage"),
                    match verified {
                        Some(
                            SelectorValue::Exact(value)
                            | SelectorValue::FilesystemSubtree(value)
                            | SelectorValue::UrlSubtree(value)
                            | SelectorValue::UrlOrigin(value),
                        ) => format!("Host verified · {}", literal(value)),
                        _ => OPAQUE.into(),
                    },
                );
            }
            PermissionResourceSelector::RemoteResource { identity, scope }
            | PermissionResourceSelector::RemoteSubtree { identity, scope } => {
                self.remote(&format!("{prefix} · Selector binding"), identity);
                self.add(format!("{prefix} · Scope keys"), scope.len().to_string());
                for (index, key) in scope.iter().enumerate() {
                    self.literal(format!("{prefix} · Scope key {}", index + 1), key, false);
                }
            }
            PermissionResourceSelector::Any => {
                self.add(format!("{prefix} · Value"), "UNRESTRICTED")
            }
        }
    }

    fn pattern(&mut self, prefix: &str, pattern: &PatternDefinition) {
        self.add(
            format!("{prefix} · Template version"),
            pattern.version.to_string(),
        );
        self.literal(format!("{prefix} · Template name"), &pattern.name, false);
        for (label, value) in pattern.context.fields() {
            self.literal(format!("{prefix} · Context {label}"), value, false);
        }
        for (index, token) in pattern.argv.iter().enumerate() {
            let token_prefix = format!("{prefix} · argv[{index}]");
            let role = match token {
                PatternToken::Exact { value, role } => {
                    self.literal(
                        format!("{token_prefix} · Fixed value"),
                        value,
                        matches!(role, ArgumentRole::Sensitive | ArgumentRole::Payload),
                    );
                    role
                }
                PatternToken::Slot { id, role } => {
                    self.add(
                        format!("{token_prefix} · Linked slot"),
                        format!("Slot #{}", id.0),
                    );
                    role
                }
            };
            self.add(format!("{token_prefix} · Role"), format!("{role:?}"));
        }
        for slot in &pattern.slots {
            let slot_prefix = format!("{prefix} · Slot #{}", slot.id.0);
            let hidden = pattern.argv.iter().any(|token| matches!(token, PatternToken::Slot { id, role: ArgumentRole::Sensitive | ArgumentRole::Payload } if *id == slot.id));
            self.literal(format!("{slot_prefix} · Label"), &slot.label, false);
            let positions = pattern
                .argv
                .iter()
                .enumerate()
                .filter_map(|(index, token)| match token {
                    PatternToken::Slot { id, .. } if *id == slot.id => {
                        Some(format!("argv[{index}]"))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.add(
                format!("{slot_prefix} · Repeated-slot equality"),
                format!("Same value at {}", positions.join(" = ")),
            );
            self.add(
                format!("{slot_prefix} · Option-like values"),
                match slot.option_like {
                    OptionLikePolicy::Reject => "Rejected",
                    OptionLikePolicy::AllowForProvenData => "Allowed for proven data",
                },
            );
            let domain = match &slot.domain {
                ArgumentDomain::ObservedSet { values } => {
                    for (index, value) in values.iter().enumerate() {
                        self.literal(
                            format!("{slot_prefix} · Allowed value {}", index + 1),
                            value,
                            hidden,
                        );
                    }
                    "Allowed value set · not evidence of execution"
                }
                ArgumentDomain::Exact { value } => {
                    self.literal(format!("{slot_prefix} · Exact value"), value, hidden);
                    "Exact literal"
                }
                ArgumentDomain::Glob { pattern } => {
                    self.literal(format!("{slot_prefix} · Glob"), pattern, hidden);
                    "Glob"
                }
                ArgumentDomain::Regex { pattern } => {
                    self.literal(format!("{slot_prefix} · Regex"), pattern, hidden);
                    "Regex"
                }
                ArgumentDomain::AnyLiteralArgument => "ANY literal argument",
            };
            self.add(format!("{slot_prefix} · Domain"), domain);
        }
        let combinations = match &pattern.combinations {
            SlotCombinations::Independent => INDEPENDENT,
            SlotCombinations::ObservedTuples { tuples } => {
                for (index, tuple) in tuples.iter().enumerate() {
                    for (id, value) in tuple {
                        let hidden = pattern.argv.iter().any(|token| matches!(token, PatternToken::Slot { id: token_id, role: ArgumentRole::Sensitive | ArgumentRole::Payload } if token_id == id));
                        self.literal(
                            format!("{prefix} · Allowed tuple {} · Slot #{}", index + 1, id.0),
                            value,
                            hidden,
                        );
                    }
                }
                TUPLES_ONLY
            }
        };
        self.add(format!("{prefix} · Combinations"), combinations);
    }

    fn json(&mut self, prefix: &str, value: &Value, hidden: bool) {
        if hidden || value.as_str().is_some_and(sensitive_text) {
            self.add(
                prefix,
                format!("{REDACTED_VALUE} {}", canonical_json_sha256(value)),
            );
            return;
        }
        match value {
            Value::Object(fields) => {
                self.add(format!("{prefix} · Type"), "Object");
                for (key, value) in fields {
                    self.json(
                        &format!("{prefix}/{}", literal(key)),
                        value,
                        likely_secret_key(key),
                    );
                }
            }
            Value::Array(values) => {
                self.add(format!("{prefix} · Type"), "Array");
                for (index, value) in values.iter().enumerate() {
                    self.json(&format!("{prefix}[{index}]"), value, false);
                }
            }
            Value::String(value) => self.literal(prefix, value, false),
            Value::Null => self.add(prefix, "null"),
            Value::Bool(value) => self.add(prefix, value.to_string()),
            Value::Number(value) => self.add(prefix, value.to_string()),
        }
    }

    fn arguments(
        &mut self,
        arguments: &PermissionArgumentConstraint,
        normalized: Option<&NormalizedPermissionDraft>,
    ) -> bool {
        let input = verified_input(normalized, arguments);
        let mode = match arguments {
            PermissionArgumentConstraint::Exact { digest } => {
                self.add("Arguments · SHA-256", digest.clone());
                if let Some(input) = input {
                    self.json(INPUT_VALUES, input, false);
                }
                "Exact input"
            }
            PermissionArgumentConstraint::SelectedDigest { pointers, digest } => {
                self.add("Arguments · SHA-256", digest.clone());
                for (index, pointer) in pointers.iter().enumerate() {
                    self.literal(format!("Arguments · Pointer {}", index + 1), pointer, false);
                    if let Some(input) = input {
                        let prefix = format!("{SELECTED_VALUE}{} · Value", literal(pointer));
                        match selected_input_pointer(input, pointer) {
                            Ok(value) => self.json(&prefix, value, sensitive_pointer(pointer)),
                            Err(_) => self.add(prefix, MISSING_POINTER),
                        }
                    }
                }
                "ALL OF selected pointers"
            }
            PermissionArgumentConstraint::Selected { arguments } => {
                for argument in arguments {
                    let prefix = format!("Argument {}", literal(&argument.pointer));
                    self.add(format!("{prefix} · SHA-256"), argument.digest.clone());
                    self.json(
                        &format!("{prefix} · Value"),
                        &argument.value,
                        sensitive_pointer(&argument.pointer),
                    );
                }
                "ALL OF selected arguments"
            }
            PermissionArgumentConstraint::Unconstrained => "UNCONSTRAINED input",
        };
        self.add("Arguments · Match", mode);
        let digested = matches!(
            arguments,
            PermissionArgumentConstraint::Exact { .. }
                | PermissionArgumentConstraint::SelectedDigest { .. }
        );
        if digested {
            self.add(
                "Arguments · Preimage",
                if input.is_some() {
                    VERIFIED_INPUT
                } else {
                    OPAQUE
                },
            );
        }
        digested && input.is_none()
    }

    fn rule(
        &mut self,
        rule: &StructuredPermissionRule,
        normalized: Option<&NormalizedPermissionDraft>,
    ) {
        self.identity(rule);
        self.add("Effect", effect_name(&rule.effect));
        self.add("Lifetime", lifetime_name(&rule.lifetime));
        self.add(
            "Targets",
            if rule.resources.is_empty() {
                "UNRESTRICTED resources"
            } else {
                "ANY OF target predicates; all requested resources must be covered"
            },
        );
        for (index, resource) in rule.resources.iter().enumerate() {
            self.resource(index, resource, normalized);
        }
        self.arguments(&rule.arguments, normalized);
    }
}

fn sensitive_pointer(pointer: &str) -> bool {
    pointer
        .split('/')
        .any(|part| likely_secret_key(&part.replace("~1", "/").replace("~0", "~")))
}

fn verified_input<'a>(
    normalized: Option<&'a NormalizedPermissionDraft>,
    arguments: &PermissionArgumentConstraint,
) -> Option<&'a Value> {
    let normalized = normalized?;
    if normalized.rule.arguments != *arguments {
        return None;
    }
    normalized
        .verified
        .iter()
        .find_map(|verified| match &verified.value {
            VerifiedValue::Input(input)
                if verified.field == EditField::Arguments
                    && argument_constraint_matches(arguments, input) =>
            {
                Some(input)
            }
            _ => None,
        })
}

fn verified_selector<'a>(
    normalized: Option<&'a NormalizedPermissionDraft>,
    field: &EditField,
    selector: &PermissionResourceSelector,
) -> Option<&'a SelectorValue> {
    let normalized = normalized?;
    let actual = match field {
        EditField::Resource(index) => &normalized.rule.resources.get(*index)?.selector,
        EditField::Attribute { resource, name } => normalized
            .rule
            .resources
            .get(*resource)?
            .attributes
            .get(name)?,
        _ => return None,
    };
    if actual != selector {
        return None;
    }
    normalized
        .verified
        .iter()
        .find_map(|verified| match &verified.value {
            VerifiedValue::Selector(value) if &verified.field == field => Some(value),
            _ => None,
        })
}

pub(super) struct PredicateDiff {
    pub label: String,
    pub before: String,
    pub after: String,
    pub changed: bool,
}

pub(super) struct ChangeReview {
    pub title: String,
    pub predicates: Vec<PredicateDiff>,
}

impl ChangeReview {
    pub fn new(
        change: &SemanticChange,
        original: Option<&StructuredPermissionRule>,
        normalized: Option<&NormalizedPermissionDraft>,
    ) -> Self {
        let (mut before, mut after) = (Facts::default(), Facts::default());
        let mut opaque_input = [false; 2];
        let title = match change {
            SemanticChange::Resource {
                index,
                before: left,
                after: right,
            } => {
                if let Some(left) = left {
                    before.resource(*index, left, normalized);
                }
                if let Some(right) = right {
                    after.resource(*index, right, normalized);
                }
                format!("Target {}", index + 1)
            }
            SemanticChange::Arguments {
                before: left,
                after: right,
            } => {
                opaque_input = [
                    before.arguments(left, normalized),
                    after.arguments(right, normalized),
                ];
                "Arguments".into()
            }
            SemanticChange::Identity => {
                if let Some(rule) = original {
                    before.identity(rule);
                }
                if let Some(normalized) = normalized {
                    after.identity(&normalized.rule);
                }
                "Identity".into()
            }
            SemanticChange::Created | SemanticChange::Revoked => {
                if let Some(rule) = original {
                    before.rule(rule, normalized);
                }
                if let Some(normalized) = normalized {
                    after.rule(&normalized.rule, Some(normalized));
                    after.add(
                        "Project",
                        normalized
                            .project
                            .as_ref()
                            .map_or_else(|| ABSENT.into(), |path| literal(&path.to_string_lossy())),
                    );
                    after.add(
                        "Label",
                        normalized
                            .label
                            .as_ref()
                            .map_or_else(|| ABSENT.into(), |label| literal(label)),
                    );
                }
                "Rule".into()
            }
            SemanticChange::Effect {
                before: left,
                after: right,
            } => {
                before.add("Effect", effect_name(left));
                after.add("Effect", effect_name(right));
                "Effect".into()
            }
            SemanticChange::Lifetime {
                before: left,
                after: right,
            } => {
                before.add("Lifetime", lifetime_name(left));
                after.add("Lifetime", lifetime_name(right));
                "Lifetime".into()
            }
            SemanticChange::Project {
                before: left,
                after: right,
            } => {
                if let Some(left) = left {
                    before.literal("Project", &left.to_string_lossy(), false);
                }
                if let Some(right) = right {
                    after.literal("Project", &right.to_string_lossy(), false);
                }
                "Project".into()
            }
            SemanticChange::Label {
                before: left,
                after: right,
            } => {
                if let Some(left) = left {
                    before.literal("Label", left, false);
                }
                if let Some(right) = right {
                    after.literal("Label", right, false);
                }
                "Label".into()
            }
        };
        let keys = before
            .0
            .keys()
            .chain(after.0.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut predicates: Vec<_> = keys
            .into_iter()
            .map(|label| {
                let left = before.0.get(&label);
                let right = after.0.get(&label);
                let changed = left != right;
                let input_value =
                    label.starts_with(INPUT_VALUES) || label.starts_with(SELECTED_VALUE);
                PredicateDiff {
                    label,
                    before: left.map_or_else(
                        || {
                            if input_value && opaque_input[0] {
                                OPAQUE
                            } else {
                                ABSENT
                            }
                            .into()
                        },
                        |value| value.display(changed, false),
                    ),
                    after: right.map_or_else(
                        || {
                            if input_value && opaque_input[1] {
                                OPAQUE
                            } else {
                                ABSENT
                            }
                            .into()
                        },
                        |value| value.display(changed, true),
                    ),
                    changed,
                }
            })
            .collect();
        predicates.sort_by_key(|predicate| !predicate.changed);
        if predicates.is_empty() {
            predicates.push(PredicateDiff {
                label: title.clone(),
                before: "Not supplied by preview".into(),
                after: "Not supplied by preview".into(),
                changed: false,
            });
        }
        Self { title, predicates }
    }

    pub fn requirement(label: String) -> Self {
        Self {
            title: "Review required".into(),
            predicates: vec![PredicateDiff {
                label: label.clone(),
                before: "Not acknowledged".into(),
                after: label,
                changed: true,
            }],
        }
    }

    pub fn summary(&self) -> (String, String, String) {
        let predicate = &self.predicates[0];
        (
            format!(
                "{} · {} changed",
                self.title,
                self.predicates
                    .iter()
                    .filter(|predicate| predicate.changed)
                    .count()
            ),
            predicate.before.clone(),
            predicate.after.clone(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Before,
    After,
}

impl Side {
    fn index(&self) -> usize {
        match self {
            Self::Before => 0,
            Self::After => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DetailControl {
    Previous,
    Next,
    Pane(Side),
    Scroll(Side, i32),
}

impl DetailControl {
    pub fn side(&self) -> Option<&Side> {
        match self {
            Self::Pane(side) | Self::Scroll(side, _) => Some(side),
            _ => None,
        }
    }
}

pub(super) struct ChangeView {
    pub change: Option<usize>,
    predicate: usize,
    scroll: [ModalScroll; 2],
}

impl Default for ChangeView {
    fn default() -> Self {
        Self {
            change: None,
            predicate: 0,
            scroll: [ModalScroll::new_top(), ModalScroll::new_top()],
        }
    }
}

impl ChangeView {
    pub fn select(&mut self, change: usize) {
        if self.change != Some(change) {
            *self = Self {
                change: Some(change),
                ..Self::default()
            };
        }
    }

    pub fn activate(&mut self, control: &DetailControl, count: usize) {
        match control {
            DetailControl::Previous | DetailControl::Next if count > 0 => {
                self.predicate = (self.predicate
                    + if *control == DetailControl::Previous {
                        count - 1
                    } else {
                        1
                    })
                    % count;
                for scroll in &mut self.scroll {
                    scroll.reset();
                }
            }
            DetailControl::Scroll(side, delta) => self.scroll(side, *delta),
            _ => {}
        }
    }

    pub fn scroll(&mut self, side: &Side, delta: i32) {
        self.scroll[side.index()].scroll(delta);
    }

    pub fn handle_key(&mut self, side: &Side, key: KeyEvent) -> bool {
        self.scroll[side.index()].handle_key(key)
    }

    pub fn render(
        &mut self,
        review: &ChangeReview,
        area: Rect,
        buffer: &mut Buffer,
        theme: &Theme,
        focus: Option<&DetailControl>,
    ) -> Vec<(Rect, DetailControl)> {
        grab_scope!("permission_scope_changes", area);
        self.predicate = self
            .predicate
            .min(review.predicates.len().saturating_sub(1));
        let predicate = &review.predicates[self.predicate];
        let [navigation, label, values] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(area.height >= DETAIL_ROWS)),
            Constraint::Min(0),
        ])
        .areas(area);
        let mut hits = Vec::new();
        clipped_line(
            &format!(
                "Predicate {}/{} · {}",
                self.predicate + 1,
                review.predicates.len(),
                if predicate.changed {
                    "changed"
                } else {
                    "unchanged"
                }
            ),
            Rect {
                width: navigation.width.saturating_sub(BUTTONS_WIDTH),
                ..navigation
            },
            buffer,
            theme.panel_title,
        );
        for (index, (text, control)) in [
            ("[<]", DetailControl::Previous),
            ("[>]", DetailControl::Next),
        ]
        .into_iter()
        .enumerate()
        {
            let cell = Rect::new(
                navigation.right().saturating_sub(BUTTONS_WIDTH)
                    + index as u16 * (BUTTON_WIDTH + BUTTON_GAP),
                navigation.y,
                BUTTON_WIDTH,
                navigation.height,
            )
            .intersection(navigation);
            Paragraph::new(text)
                .style(if focus == Some(&control) {
                    theme.item_selected
                } else {
                    theme.keybind_key
                })
                .render(cell, buffer);
            hits.push((cell, control));
        }
        clipped_line(&predicate.label, label, buffer, theme.panel_title);
        let [before, _, after] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(values);
        for (side, pane, value, title, style) in [
            (
                Side::Before,
                before,
                &predicate.before,
                "Before",
                theme.diff_old,
            ),
            (
                Side::After,
                after,
                &predicate.after,
                "After",
                theme.diff_new,
            ),
        ] {
            let [header, body, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(pane);
            Paragraph::new(title)
                .style(if focus.and_then(DetailControl::side) == Some(&side) {
                    theme.item_selected
                } else {
                    style
                })
                .render(header, buffer);
            for (index, (text, delta)) in [("[^]", PAGE_ROWS), ("[v]", -PAGE_ROWS)]
                .into_iter()
                .enumerate()
            {
                let cell = Rect::new(
                    header.right().saturating_sub(BUTTONS_WIDTH)
                        + index as u16 * (BUTTON_WIDTH + BUTTON_GAP),
                    header.y,
                    BUTTON_WIDTH,
                    header.height,
                )
                .intersection(header);
                Paragraph::new(text)
                    .style(theme.keybind_key)
                    .render(cell, buffer);
                hits.push((cell, DetailControl::Scroll(side.clone(), delta)));
            }
            hits.push((pane, DetailControl::Pane(side.clone())));
            let paragraph = Paragraph::new(vec![
                Line::from(value.clone()),
                Line::styled(predicate.label.clone(), theme.item_desc),
            ])
            .style(style)
            .wrap(Wrap { trim: false });
            let total = u16::try_from(paragraph.line_count(body.width.max(1))).unwrap_or(u16::MAX);
            let scroll = &mut self.scroll[side.index()];
            scroll.update_dimensions(total, body.height);
            let offset = scroll.offset();
            paragraph.scroll((offset, 0)).render(body, buffer);
            clipped_line(
                &format!(
                    "{}{}-{}/{}{}",
                    if offset > 0 { "↑ " } else { "" },
                    offset.saturating_add(1),
                    offset.saturating_add(body.height).min(total),
                    total,
                    if offset.saturating_add(body.height) < total {
                        " ↓"
                    } else {
                        ""
                    }
                ),
                footer,
                buffer,
                theme.item_desc,
            );
        }
        hits.retain(|(area, _)| !area.is_empty());
        hits
    }
}

pub(super) fn clipped_line(text: &str, area: Rect, buffer: &mut Buffer, style: Style) {
    if text.width() <= usize::from(area.width) {
        Paragraph::new(text).style(style).render(area, buffer);
        return;
    }
    let mut clipped = String::new();
    let mut width = 0;
    let line = Line::from(text);
    for grapheme in line.styled_graphemes(style) {
        width += grapheme.symbol.width();
        if width >= usize::from(area.width) {
            break;
        }
        clipped.push_str(grapheme.symbol);
    }
    clipped.push('…');
    Paragraph::new(clipped).style(style).render(area, buffer);
}

#[cfg(test)]
mod tests {
    use caudra_agent::permissions::editor::{
        EditField, NormalizedPermissionDraft, SelectorValue, SemanticChange, VerifiedField,
        VerifiedValue,
    };
    use caudra_agent::permissions::{canonical_json_sha256, selected_input, selected_input_digest};
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, OptionLikePolicy, PatternToken, SlotCombinations, SlotId,
    };
    use caudra_storage::permission_state::{
        PermissionArgumentConstraint, PermissionResourceConstraint, PermissionResourceKind,
        PermissionResourceSelector, RemotePermissionIdentity,
    };
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, SourceTrustAnchor,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use test_case::test_case;

    use super::{
        ABSENT, AFTER_SECRET, BEFORE_SECRET, ChangeReview, INDEPENDENT, MISSING_POINTER, OPAQUE,
        REDACTED_VALUE, TUPLES_ONLY, VERIFIED_INPUT,
    };
    use crate::components::permission_scope::model::literal;
    use crate::components::permission_scope::tests::record;

    const DIGEST_BEFORE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
    const DIGEST_AFTER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2";
    const WORKDIR: &str = "workdir";
    const VERIFIED_PATH: &str = "/verified/new/workdir";
    const UNVERIFIED_PATH: &str = "/display/only/not/a/preimage";
    const NEW_VALUE: &str = "new allowed value";
    const SECRET_BEFORE: &str = "private-before-payload";
    const SECRET_AFTER: &str = "private-after-payload";
    const QUERY: SlotId = SlotId(1);
    const PATH: SlotId = SlotId(2);
    const REMOTE_KEY: &str = "opaque/project/component";
    const REMOTE_PRINCIPAL: &str = "old-principal";
    const REMOTE_PROJECT: &str = "old-project";
    const IGNORED_INPUT: &str = "unselected-field-is-not-authority";
    const WORKDIR_POINTER: &str = "/workdir";
    const NESTED_POINTER: &str = "/nested";
    const ESCAPED_NULL_POINTER: &str = "/a~1b/~0key/0";
    const ESCAPED_NUMBER_POINTER: &str = "/a~1b/~0key/1";
    const ABSENT_POINTER: &str = "/absent";
    const SECRET_POINTERS: [&str; 2] = ["/auth", "/command"];

    fn input_preview(input: Value, pointers: &[&str]) -> NormalizedPermissionDraft {
        let mut record = record(false);
        record.rule.arguments = if pointers.is_empty() {
            PermissionArgumentConstraint::Exact {
                digest: canonical_json_sha256(&input),
            }
        } else {
            PermissionArgumentConstraint::SelectedDigest {
                pointers: pointers.iter().map(|pointer| (*pointer).into()).collect(),
                digest: selected_input_digest(&input, pointers).unwrap(),
            }
        };
        let mut review = record.review.unwrap();
        review.input = Some(json!({"workdir": UNVERIFIED_PATH}));
        NormalizedPermissionDraft {
            rule: record.rule,
            project: None,
            label: None,
            review,
            verified: vec![VerifiedField {
                field: EditField::Arguments,
                value: VerifiedValue::Input(input),
            }],
            opaque: Vec::new(),
        }
    }

    fn input_change(normalized: &NormalizedPermissionDraft) -> SemanticChange {
        SemanticChange::Arguments {
            before: PermissionArgumentConstraint::Exact {
                digest: DIGEST_BEFORE.into(),
            },
            after: normalized.rule.arguments.clone(),
        }
    }

    #[test_case(false; "exact_json")]
    #[test_case(true; "selected_pointers_only")]
    fn verified_argument_preimages_show_typed_values_and_keep_old_input_opaque(selected: bool) {
        let pointers = if selected {
            vec![WORKDIR_POINTER, NESTED_POINTER]
        } else {
            Vec::new()
        };
        let normalized = input_preview(
            json!({"workdir": VERIFIED_PATH, "nested": {"enabled": true, "limit": 7}, "ignored": IGNORED_INPUT}),
            &pointers,
        );
        let review = ChangeReview::new(&input_change(&normalized), None, Some(&normalized));
        let path = review
            .predicates
            .iter()
            .find(|predicate| predicate.after == literal(VERIFIED_PATH))
            .unwrap();
        assert!(path.label.contains(WORKDIR));
        assert_eq!(path.before, OPAQUE);
        assert!(
            review
                .predicates
                .iter()
                .any(|predicate| predicate.after == VERIFIED_INPUT)
        );
        assert!(
            review
                .predicates
                .iter()
                .any(|predicate| predicate.label.contains("enabled")
                    && predicate.after == true.to_string())
        );
        assert!(
            review
                .predicates
                .iter()
                .any(|predicate| predicate.label.contains("limit")
                    && predicate.after == 7.to_string())
        );
        assert!(
            review
                .predicates
                .iter()
                .all(|predicate| !predicate.before.contains(UNVERIFIED_PATH)
                    && !predicate.after.contains(UNVERIFIED_PATH))
        );
        assert_eq!(
            review
                .predicates
                .iter()
                .any(|predicate| predicate.after.contains(IGNORED_INPUT)),
            !selected
        );
    }

    #[test_case("missing"; "no_host_field")]
    #[test_case("field"; "wrong_host_field")]
    #[test_case("constraint"; "stale_constraint")]
    #[test_case("input"; "mismatched_input_digest")]
    fn argument_preimages_require_a_matching_verified_input(case: &str) {
        let mut normalized = input_preview(json!({"workdir": VERIFIED_PATH}), &[]);
        let change = input_change(&normalized);
        match case {
            "missing" => normalized.verified.clear(),
            "field" => normalized.verified[0].field = EditField::Resource(0),
            "constraint" => normalized.rule.arguments = PermissionArgumentConstraint::Unconstrained,
            "input" => {
                normalized.verified[0].value =
                    VerifiedValue::Input(json!({"workdir": UNVERIFIED_PATH}))
            }
            _ => unreachable!(),
        }
        let review = ChangeReview::new(&change, None, Some(&normalized));
        assert!(
            review
                .predicates
                .iter()
                .any(|predicate| predicate.after == OPAQUE)
        );
        assert!(
            review
                .predicates
                .iter()
                .all(|predicate| !predicate.after.contains(VERIFIED_PATH)
                    && !predicate.after.contains(UNVERIFIED_PATH))
        );
    }

    #[test_case(ESCAPED_NULL_POINTER, "null"; "null_is_present")]
    #[test_case(ESCAPED_NUMBER_POINTER, "7"; "escaped_pointer_value")]
    #[test_case(ABSENT_POINTER, MISSING_POINTER; "missing_is_not_null")]
    fn selected_argument_preimages_preserve_missing_null_and_pointer_boundaries(
        pointer: &str,
        expected: &str,
    ) {
        let normalized = input_preview(
            json!({"a/b": {"~key": [null, 7]}, "unselected": IGNORED_INPUT}),
            &[pointer],
        );
        let review = ChangeReview::new(&input_change(&normalized), None, Some(&normalized));
        let value = review
            .predicates
            .iter()
            .find(|predicate| predicate.label == format!("Argument {} · Value", literal(pointer)))
            .unwrap();
        assert_eq!(value.after, expected);
        assert_eq!(value.before, OPAQUE);
        assert!(
            review
                .predicates
                .iter()
                .all(|predicate| !predicate.after.contains(IGNORED_INPUT))
        );
    }

    #[test_case("exact"; "exact_input")]
    #[test_case("selected"; "selected_input")]
    #[test_case("legacy"; "legacy_selected_values")]
    fn argument_secrets_keep_distinct_opaque_identities_without_plaintext(mode: &str) {
        let input = |secret| json!({"auth": {"api_key": secret}, "command": format!("deploy --token {secret}")});
        let pointers = if mode == "exact" {
            Vec::new()
        } else {
            SECRET_POINTERS.to_vec()
        };
        let mut identities = Vec::new();
        for secret in [SECRET_BEFORE, SECRET_AFTER] {
            let normalized = input_preview(input(secret), &pointers);
            let change = if mode == "legacy" {
                SemanticChange::Arguments {
                    before: PermissionArgumentConstraint::Selected {
                        arguments: selected_input(&input(SECRET_BEFORE), &SECRET_POINTERS).unwrap(),
                    },
                    after: normalized.rule.arguments.clone(),
                }
            } else {
                input_change(&normalized)
            };
            let review = ChangeReview::new(&change, None, Some(&normalized));
            let value = review
                .predicates
                .iter()
                .find(|predicate| predicate.label.contains("api_key"))
                .unwrap();
            assert_eq!(
                value.after,
                format!(
                    "{REDACTED_VALUE} {}",
                    canonical_json_sha256(&Value::String(secret.into()))
                )
            );
            identities.push(value.after.clone());
            assert!(review.predicates.iter().all(|predicate| {
                [SECRET_BEFORE, SECRET_AFTER].iter().all(|secret| {
                    !predicate.before.contains(*secret) && !predicate.after.contains(*secret)
                })
            }));
        }
        assert_ne!(identities[0], identities[1]);
    }

    fn resource_change(
        before: PermissionResourceConstraint,
        after: PermissionResourceConstraint,
    ) -> SemanticChange {
        SemanticChange::Resource {
            index: 0,
            before: Some(Box::new(before)),
            after: Some(Box::new(after)),
        }
    }

    #[test_case("access", "Access"; "access_wildcard")]
    #[test_case("protection", "Protection"; "protection_wildcard")]
    #[test_case("attribute", "SHA-256"; "attribute_identity_same_digest_prefix")]
    #[test_case("attribute_mode", "Match"; "attribute_match_mode")]
    #[test_case("attribute_name", "Attribute"; "attribute_names_same_count")]
    #[test_case("digest", "SHA-256"; "target_identity_same_digest_prefix")]
    #[test_case("domain", "Domain"; "unrestricted_slot_domain")]
    #[test_case("values", "Allowed value"; "allowed_values_same_count")]
    #[test_case("glob", "Glob"; "changed_glob")]
    #[test_case("regex", "Regex"; "changed_regex")]
    #[test_case("options", "Option-like values"; "option_like_authority")]
    #[test_case("links", "Repeated-slot equality"; "repeated_slot_relationship")]
    #[test_case("tuples", "Allowed tuple"; "tuple_values_same_count")]
    #[test_case("independent", "Combinations"; "tuple_independence")]
    #[test_case("context", "Context"; "execution_context")]
    fn resource_authority_differences_never_collapse(case: &str, label: &str) {
        let mut before = record(true).rule.resources.remove(0);
        if case == "digest" {
            before.selector = PermissionResourceSelector::Digest {
                digest: DIGEST_BEFORE.into(),
            };
        }
        before.attributes.insert(
            WORKDIR.into(),
            PermissionResourceSelector::Digest {
                digest: DIGEST_BEFORE.into(),
            },
        );
        if case == "regex" {
            let PermissionResourceSelector::CommandTemplate { definition } = &mut before.selector
            else {
                unreachable!()
            };
            definition.slots[1].domain = ArgumentDomain::Regex {
                pattern: "^src/[^/]+$".into(),
            };
        }
        let mut after = before.clone();
        match case {
            "access" => after.access = None,
            "protection" => after.protected = None,
            "attribute" => {
                after.attributes.insert(
                    WORKDIR.into(),
                    PermissionResourceSelector::Digest {
                        digest: DIGEST_AFTER.into(),
                    },
                );
            }
            "attribute_mode" => {
                after
                    .attributes
                    .insert(WORKDIR.into(), PermissionResourceSelector::Any);
            }
            "attribute_name" => {
                let value = after.attributes.remove(WORKDIR).unwrap();
                after.attributes.insert(NEW_VALUE.into(), value);
            }
            "digest" => {
                after.selector = PermissionResourceSelector::Digest {
                    digest: DIGEST_AFTER.into(),
                }
            }
            _ => {
                let PermissionResourceSelector::CommandTemplate { definition } =
                    &mut after.selector
                else {
                    unreachable!()
                };
                match case {
                    "domain" => definition.slots[0].domain = ArgumentDomain::AnyLiteralArgument,
                    "values" => {
                        definition.slots[0].domain = ArgumentDomain::ObservedSet {
                            values: [NEW_VALUE.into(), "error".into()].into(),
                        }
                    }
                    "glob" => {
                        definition.slots[1].domain = ArgumentDomain::Glob {
                            pattern: "**".into(),
                        }
                    }
                    "regex" => {
                        definition.slots[1].domain = ArgumentDomain::Regex {
                            pattern: ".*".into(),
                        }
                    }
                    "options" => {
                        definition.slots[0].option_like = OptionLikePolicy::AllowForProvenData
                    }
                    "links" => {
                        definition.argv[4] = PatternToken::Slot {
                            id: PATH,
                            role: ArgumentRole::Data,
                        }
                    }
                    "tuples" => {
                        let SlotCombinations::ObservedTuples { tuples } =
                            &mut definition.combinations
                        else {
                            unreachable!()
                        };
                        let mut tuple = tuples.pop_first().unwrap();
                        tuple.insert(QUERY, NEW_VALUE.into());
                        tuples.insert(tuple);
                    }
                    "independent" => definition.combinations = SlotCombinations::Independent,
                    "context" => definition.context.effective_workdir = VERIFIED_PATH.into(),
                    _ => unreachable!(),
                }
            }
        }
        let review = ChangeReview::new(&resource_change(before, after), None, None);
        assert!(
            review
                .predicates
                .iter()
                .any(|predicate| predicate.label.contains(label)
                    && predicate.changed
                    && predicate.before != predicate.after)
        );
        let (_, before, after) = review.summary();
        assert_ne!(before, after);
        assert!(
            review
                .predicates
                .iter()
                .filter(|predicate| predicate.changed)
                .all(|predicate| predicate.before != predicate.after)
        );
        if case == "independent" {
            let combinations = review
                .predicates
                .iter()
                .find(|predicate| predicate.label.ends_with("Combinations"))
                .unwrap();
            assert_eq!(combinations.before, TUPLES_ONLY);
            assert_eq!(combinations.after, INDEPENDENT);
            assert!(
                review
                    .predicates
                    .iter()
                    .any(|predicate| predicate.label.contains("Allowed tuple")
                        && predicate.after == ABSENT)
            );
        }
    }

    #[test_case(false; "target_preimage")]
    #[test_case(true; "attribute_preimage")]
    fn verified_preimages_bind_to_the_exact_predicate_not_review_labels(attribute: bool) {
        let record = record(false);
        let mut before = record.rule.resources[0].clone();
        before.selector = PermissionResourceSelector::Digest {
            digest: DIGEST_BEFORE.into(),
        };
        before
            .attributes
            .insert(WORKDIR.into(), before.selector.clone());
        let mut after = before.clone();
        let changed = PermissionResourceSelector::Digest {
            digest: DIGEST_AFTER.into(),
        };
        let (field, prefix) = if attribute {
            after.attributes.insert(WORKDIR.into(), changed);
            (
                EditField::Attribute {
                    resource: 0,
                    name: WORKDIR.into(),
                },
                "Target 1 · Attribute \"workdir\"",
            )
        } else {
            after.selector = changed;
            (EditField::Resource(0), "Target 1")
        };
        let mut normalized = NormalizedPermissionDraft {
            rule: record.rule,
            project: None,
            label: None,
            review: record.review.unwrap(),
            verified: vec![VerifiedField {
                field,
                value: VerifiedValue::Selector(SelectorValue::Exact(VERIFIED_PATH.into())),
            }],
            opaque: Vec::new(),
        };
        normalized.rule.resources = vec![after.clone()];
        normalized.review.resources[0].value = Some(UNVERIFIED_PATH.into());
        normalized.review.resources[0].attributes =
            BTreeMap::from([(WORKDIR.into(), UNVERIFIED_PATH.into())]);
        let change = resource_change(before, after);
        let review = ChangeReview::new(&change, None, Some(&normalized));
        let preimage = review
            .predicates
            .iter()
            .find(|predicate| predicate.label == format!("{prefix} · Preimage"))
            .unwrap();
        assert_eq!(preimage.before, OPAQUE);
        assert!(preimage.after.contains(VERIFIED_PATH));
        assert!(
            review
                .predicates
                .iter()
                .all(|predicate| !predicate.before.contains(UNVERIFIED_PATH)
                    && !predicate.after.contains(UNVERIFIED_PATH))
        );
        normalized.rule.resources[0].selector = PermissionResourceSelector::Any;
        normalized.rule.resources[0].attributes.clear();
        let stale = ChangeReview::new(&change, None, Some(&normalized));
        assert!(
            stale
                .predicates
                .iter()
                .all(|predicate| !predicate.before.contains(VERIFIED_PATH)
                    && !predicate.after.contains(VERIFIED_PATH))
        );
    }

    #[test_case(1; "one")]
    #[test_case(8; "several")]
    #[test_case(32; "all_attribute_rows")]
    fn every_attribute_has_its_own_noncollapsed_predicate(count: usize) {
        let mut before = record(false).rule.resources.remove(0);
        before.attributes = (0..count)
            .map(|index| {
                (
                    format!("attribute-{index}"),
                    PermissionResourceSelector::Digest {
                        digest: DIGEST_BEFORE.into(),
                    },
                )
            })
            .collect();
        let mut after = before.clone();
        for selector in after.attributes.values_mut() {
            *selector = PermissionResourceSelector::Digest {
                digest: DIGEST_AFTER.into(),
            };
        }
        let review = ChangeReview::new(&resource_change(before, after), None, None);
        for index in 0..count {
            let label = format!("Target 1 · Attribute \"attribute-{index}\" · SHA-256");
            let predicate = review
                .predicates
                .iter()
                .find(|predicate| predicate.label == label)
                .unwrap();
            assert_eq!(predicate.before, DIGEST_BEFORE);
            assert_eq!(predicate.after, DIGEST_AFTER);
            assert!(predicate.changed);
        }
    }

    #[test_case("principal"; "principal_identity")]
    #[test_case("project"; "project_identity")]
    #[test_case("scope"; "opaque_scope_key")]
    #[test_case("kind"; "same_kind_label_different_type")]
    fn remote_predicate_identities_are_not_display_path_aliases(part: &str) {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new(WORKDIR).unwrap(),
            WORKDIR,
            WORKDIR,
            WORKDIR,
            WORKDIR,
        )
        .unwrap();
        let identity = RemotePermissionIdentity {
            principal: AuthenticatedPrincipalId::new(authority.clone(), REMOTE_PRINCIPAL).unwrap(),
            project: ProjectIdentity::new(
                authority.clone(),
                ProjectKey::new(REMOTE_PROJECT).unwrap(),
            ),
            authority,
        };
        let mut before = record(false).rule.resources.remove(0);
        before.kind = PermissionResourceKind::RemoteFile {
            identity: identity.clone(),
        };
        before.selector = PermissionResourceSelector::RemoteSubtree {
            identity: identity.clone(),
            scope: vec![REMOTE_KEY.into()],
        };
        let mut after = before.clone();
        let mut changed_identity = identity;
        match part {
            "principal" => {
                changed_identity.principal =
                    AuthenticatedPrincipalId::new(changed_identity.authority.clone(), NEW_VALUE)
                        .unwrap()
            }
            "project" => {
                changed_identity.project = ProjectIdentity::new(
                    changed_identity.authority.clone(),
                    ProjectKey::new(NEW_VALUE).unwrap(),
                )
            }
            _ => {}
        }
        after.kind = if part == "kind" {
            PermissionResourceKind::RemoteResource {
                identity: changed_identity.clone(),
                resource_kind: "file".into(),
            }
        } else {
            PermissionResourceKind::RemoteFile {
                identity: changed_identity.clone(),
            }
        };
        after.selector = PermissionResourceSelector::RemoteSubtree {
            identity: changed_identity,
            scope: vec![
                if part == "scope" {
                    NEW_VALUE
                } else {
                    REMOTE_KEY
                }
                .into(),
            ],
        };
        let review = ChangeReview::new(&resource_change(before, after), None, None);
        let (_, left, right) = review.summary();
        assert_ne!(left, right);
        assert!(
            review
                .predicates
                .iter()
                .any(|predicate| predicate.changed && predicate.before != predicate.after)
        );
    }

    #[test_case(ArgumentRole::Sensitive; "sensitive")]
    #[test_case(ArgumentRole::Payload; "payload")]
    fn redacted_changes_remain_distinct_without_disclosing_literals(role: ArgumentRole) {
        let mut before = record(true).rule.resources.remove(0);
        let PermissionResourceSelector::CommandTemplate { definition } = &mut before.selector
        else {
            unreachable!()
        };
        definition.argv[0] = PatternToken::Exact {
            value: SECRET_BEFORE.into(),
            role: role.clone(),
        };
        let mut after = before.clone();
        let PermissionResourceSelector::CommandTemplate { definition } = &mut after.selector else {
            unreachable!()
        };
        definition.argv[0] = PatternToken::Exact {
            value: SECRET_AFTER.into(),
            role,
        };
        let review = ChangeReview::new(&resource_change(before, after), None, None);
        let changed = review
            .predicates
            .iter()
            .find(|predicate| predicate.changed)
            .unwrap();
        assert_eq!(changed.before, BEFORE_SECRET);
        assert_eq!(changed.after, AFTER_SECRET);
        assert!(review.predicates.iter().all(|predicate| {
            [SECRET_BEFORE, SECRET_AFTER].iter().all(|secret| {
                !predicate.before.contains(*secret) && !predicate.after.contains(*secret)
            })
        }));
    }
}
