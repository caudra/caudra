use std::collections::{HashSet, VecDeque};

use caudra_agent::permissions::{
    COMPOSABLE_SHELL_OPTIONS, DEFAULT_DENY_GUIDANCE, PermissionAnswer, PermissionCaution,
    PermissionLifetime, PermissionRequest, PermissionRowGrant, PermissionRuleOption,
    ResourceCoverage, StructuredPermissionEffect, grade_command_pattern,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use serde_json::{Map, Value};

use crate::components::permission_scope::view::{ScopeControl, ScopeView};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{
    ModalScroll, Overlay, escape_terminal_controls, hover_style, is_ctrl, visual_rows,
};
use crate::text_buffer::TextBuffer;
use crate::theme;

mod decision;
mod details;
mod input;
mod inspector;
mod scope;
mod view;

use decision::{Confirmation, command_ladders};
pub(crate) use details::{likely_secret_key, sensitive_text};
use input::{InputFreshness, hint_key};
use inspector::{EditedPattern, InspectorControl, PatternInspector};

const KEY_ALLOW_ONCE: &str = "y";
const KEY_ALLOW_SESSION: &str = "s";
const KEY_ALLOW_LOCAL: &str = "a";
const KEY_ALLOW_GLOBAL: &str = "A";
const KEY_GUIDE_DENY: &str = "g";
const KEY_DENY_LOCAL: &str = "d";
const KEY_DENY_GLOBAL: &str = "D";
const HINT_ENTER: &str = "Enter";
const HINT_ESC: &str = "Esc";
const HINT_CONFIRM: &str = "Enter/y";
const CHIP_ONCE: &str = "Once; not remembered";
const CHIP_COVERED: &str = "already allowed";
const COVERAGE_SEPARATOR: &str = " · ";
const MIN_REVIEW_WIDTH: u16 = 32;
const MIN_REVIEW_HEIGHT: u16 = 10;
const KEY_DETAILS: &str = "v";
const KEY_COVERED: &str = "c";

#[derive(Default, PartialEq, Eq)]
enum Panel {
    #[default]
    Main,
    Scopes,
    Details,
}

type HintPairs = Vec<(&'static str, &'static str)>;

enum FooterRow {
    Hints(HintPairs),
    Guidance,
    ConfirmationInput,
    InspectorStatus,
    Rearm,
}

struct PromptBody {
    lines: Vec<Line<'static>>,
    entries: Vec<(PromptTarget, u16)>,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct RowChoice {
    rung: usize,
    written: Option<String>,
    pattern: Option<EditedPattern>,
}

impl RowChoice {
    fn ladder_len(&self, offered: usize) -> usize {
        1 + offered + usize::from(self.written.is_some())
    }

    fn grant(&self, offered: &[&PermissionRuleOption]) -> Option<PermissionRowGrant> {
        match self.rung.checked_sub(1)? {
            rung if rung < offered.len() => Some(
                self.pattern
                    .as_ref()
                    .filter(|edited| edited.option_id == offered[rung].id)
                    .map_or_else(
                        || PermissionRowGrant::Offered(offered[rung].id.clone()),
                        |edited| PermissionRowGrant::Pattern {
                            option_id: edited.option_id.clone(),
                            definition: edited.definition.clone(),
                        },
                    ),
            ),
            _ => self.written.clone().map(PermissionRowGrant::Written),
        }
    }
}

struct AuthorityRow {
    chosen: String,
    rungs: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PromptState {
    #[default]
    Normal,
    ConfirmAllowAlwaysLocal,
    ConfirmAllowAlwaysGlobal,
    ConfirmAllowSession,
    ConfirmDenyAlwaysLocal,
    ConfirmDenyAlwaysGlobal,
    DenyEditing,
    PatternEditing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionDecision {
    pub request_id: String,
    pub answer: PermissionAnswer,
}

struct QueuedPermission {
    request: Box<PermissionRequest>,
    requester: Option<String>,
}

pub struct PermissionPrompt {
    requests: VecDeque<QueuedPermission>,
    request_ids: HashSet<String>,
    state: PromptState,
    buffer: TextBuffer,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    selected_option: String,
    scopes: Vec<RowChoice>,
    pending_reveal: Option<PromptTarget>,
    row_hits: Vec<PromptHit>,
    mouse_down: Option<PromptTarget>,
    hover: Option<PromptTarget>,
    area: Rect,
    panel: Panel,
    lifetime: PermissionLifetime,
    confirmation: Option<Confirmation>,
    inspector: Option<PatternInspector>,
    expanded_covered: bool,
    focus: Option<PromptTarget>,
    awaiting_review: bool,
    input_freshness: InputFreshness,
    scope_view: ScopeView,
    scope_authority: usize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum PromptTarget {
    Scope,
    Authority(String),
    Hint(KeyEvent),
    Inspector(InspectorControl),
    VisualScope(usize, ScopeControl),
}

struct PromptHit {
    area: Rect,
    target: PromptTarget,
}

pub(crate) enum PromptMouse {
    Passthrough,
    Consumed,
    Decided(PermissionDecision),
}

impl Overlay for PermissionPrompt {
    fn is_open(&self) -> bool {
        !self.requests.is_empty()
    }

    fn is_modal(&self) -> bool {
        false
    }

    fn close(&mut self) {
        self.requests.clear();
        self.request_ids.clear();
        self.reset_view();
    }
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self {
            requests: VecDeque::new(),
            request_ids: HashSet::new(),
            state: PromptState::Normal,
            buffer: TextBuffer::new(String::new()),
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            selected_option: "allow_exact".into(),
            scopes: Vec::new(),
            pending_reveal: None,
            row_hits: Vec::new(),
            mouse_down: None,
            hover: None,
            area: Rect::default(),
            panel: Panel::Main,
            lifetime: PermissionLifetime::Once,
            confirmation: None,
            inspector: None,
            expanded_covered: false,
            focus: None,
            awaiting_review: false,
            input_freshness: InputFreshness::default(),
            scope_view: ScopeView::default(),
            scope_authority: 0,
        }
    }

    pub fn enqueue(&mut self, request: Box<PermissionRequest>, requester: Option<String>) -> bool {
        if !self.request_ids.insert(request.id.clone()) {
            return false;
        }
        let first = self.requests.is_empty();
        self.requests
            .push_back(QueuedPermission { request, requester });
        if first {
            self.reset_view();
        }
        true
    }

    pub fn update(&mut self, request: Box<PermissionRequest>) -> bool {
        let Some(index) = self
            .requests
            .iter()
            .position(|queued| queued.request.id == request.id)
        else {
            return false;
        };
        self.requests[index].request = request;
        if index == 0 {
            self.reset_view();
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn open(
        &mut self,
        id: String,
        tool: caudra_config::ToolKey,
        scopes: Vec<String>,
        subagent_id: Option<String>,
    ) {
        self.enqueue(
            Box::new(PermissionRequest::from_legacy(
                id,
                tool,
                scopes,
                Value::Null,
                std::path::Path::new("/project"),
                true,
            )),
            subagent_id,
        );
    }

    pub(crate) fn tool(&self) -> Option<&caudra_config::ToolKey> {
        self.current().map(|request| &request.tool)
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.requests.len()
    }

    pub(crate) fn request_id(&self) -> Option<&str> {
        self.current().map(|request| request.id.as_str())
    }

    #[cfg(test)]
    pub(crate) fn requester(&self) -> Option<&str> {
        self.requests
            .front()
            .and_then(|queued| queued.requester.as_deref())
    }

    pub fn resolve(&mut self, request_id: &str) -> bool {
        let Some(request) = self.requests.front() else {
            return false;
        };
        if request.request.id != request_id {
            return false;
        }
        let request = self.requests.pop_front().expect("front request exists");
        self.request_ids.remove(&request.request.id);
        self.reset_view();
        true
    }

    pub fn resolve_pending(&mut self, request_id: &str) -> bool {
        let Some(index) = self
            .requests
            .iter()
            .position(|queued| queued.request.id == request_id)
        else {
            return false;
        };
        self.requests.remove(index);
        self.request_ids.remove(request_id);
        if index == 0 {
            self.reset_view();
        }
        true
    }

    fn current(&self) -> Option<&PermissionRequest> {
        self.requests.front().map(|queued| queued.request.as_ref())
    }

    fn reset_view(&mut self) {
        self.scope_view = ScopeView::default();
        self.input_freshness.barrier();
        self.state = PromptState::Normal;
        self.panel = Panel::Main;
        self.lifetime = PermissionLifetime::Once;
        self.confirmation = None;
        self.inspector = None;
        self.expanded_covered = false;
        self.focus = None;
        self.awaiting_review = true;
        self.buffer = TextBuffer::new(String::new());
        self.scroll.reset();
        self.pending_reveal = None;
        self.row_hits.clear();
        self.mouse_down = None;
        self.hover = None;
        self.reset_selection();
    }

    fn reset_selection(&mut self) {
        let Some(request) = self.current() else {
            self.scopes.clear();
            self.selected_option = "allow_exact".into();
            return;
        };
        let covered = |row: usize| {
            request
                .presentation
                .resources
                .get(row)
                .is_some_and(|shown| shown.covered())
        };
        let ladders = command_ladders(request);
        let scopes = (0..ladders.len())
            .map(|row| RowChoice {
                rung: usize::from(!covered(row)),
                written: None,
                pattern: None,
            })
            .collect();
        let default_authority = || {
            request
                .options
                .iter()
                .find(|option| {
                    option.is_default && option.rule.effect == StructuredPermissionEffect::Allow
                })
                .map_or_else(|| "allow_exact".into(), |option| option.id.clone())
        };
        let selected = ladders
            .iter()
            .enumerate()
            .find(|(row, _)| !covered(*row))
            .or_else(|| ladders.iter().enumerate().next())
            .map_or_else(default_authority, |(_, ladder)| {
                ladder[0]
                    .group
                    .as_ref()
                    .map_or_else(|| ladder[0].id.clone(), |group| group.key.clone())
            });
        self.scopes = scopes;
        self.selected_option = selected;
        self.select_default_path();
    }
}

#[cfg(test)]
mod tests {
    use super::PermissionPrompt;
    use super::view::tests::{key, render, request};
    use crossterm::event::KeyCode;
    use serde_json::json;

    /// Whoever enqueues a request owns the wording, so the prompt renders this
    /// verbatim rather than describing the asker itself.
    const REQUESTER: &str = "subtask task-1";

    #[test]
    fn queue_is_fifo_and_deduplicated_by_request_id() {
        let mut prompt = PermissionPrompt::new();
        assert!(prompt.enqueue(request("first", json!({"n": 1})), Some(REQUESTER.into())));
        assert!(prompt.enqueue(request("second", json!({"n": 2})), None));
        assert!(!prompt.enqueue(request("first", json!({"n": 3})), None));
        assert_eq!(prompt.pending_count(), 2);
        assert_eq!(prompt.request_id(), Some("first"));
        assert!(render(&mut prompt, 100, 24).contains(REQUESTER));

        let first = prompt.handle_key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(first.request_id, "first");
        assert!(prompt.resolve(&first.request_id));
        assert_eq!(prompt.request_id(), Some("second"));
        let second = prompt.handle_key(key(KeyCode::Esc)).unwrap();
        assert_eq!(second.request_id, "second");
    }

    #[test]
    fn resolving_a_covered_request_preserves_unmatched_fifo_order() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(request("first", json!({"n": 1})), None);
        prompt.enqueue(request("covered", json!({"n": 2})), None);
        prompt.enqueue(request("third", json!({"n": 3})), None);

        assert!(prompt.resolve_pending("covered"));
        assert_eq!(prompt.pending_count(), 2);
        assert_eq!(prompt.request_id(), Some("first"));
        assert!(prompt.resolve_pending("first"));
        assert_eq!(prompt.request_id(), Some("third"));
        assert!(!prompt.resolve_pending("missing"));
    }
}
