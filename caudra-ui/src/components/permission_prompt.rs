use std::collections::{HashSet, VecDeque};

use caudra_agent::permissions::{
    PermissionAnswer, PermissionLifetime, PermissionRequest, PermissionRowGrant,
};
use caudra_config::ToolKey;
use caudra_workbench::text_field::{FieldKind, TextField};
use crossterm::event::KeyEvent;
use ratatui::layout::Rect;

use crate::components::permission_scope::view::{ScopeControl, ScopeView};
use crate::components::scrollbar::Scrollbar;
use crate::components::{ModalScroll, Overlay};

mod choices;
mod customize;
mod decision;
mod details;
mod input;
mod inspector;
mod notes;
mod scope;
mod step_through;
mod view;

use choices::Choice;
use customize::{Customize, Effect};
use decision::Pending;
pub(crate) use details::{likely_secret_key, sensitive_text};
use input::InputFreshness;
pub(crate) use inspector::pattern_widened;
use inspector::{InspectorControl, PatternInspector};
pub(crate) use notes::{origin_word, tilde};
#[cfg(test)]
pub(crate) use scope::MISSING_SCOPE;
pub(crate) use scope::{rule_names_commands, rule_phrase, rule_summary, tool_words};
use step_through::StepThrough;
pub(crate) use step_through::lifetime_phrase;
#[cfg(test)]
pub(crate) use view::tests::{THEMES, WIDTHS, assert_plain, buffer_rows};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Panel {
    #[default]
    Main,
    StepThrough,
    Customize,
    Details,
}

/// What typing goes to: nothing, the guidance under No, or a pattern being
/// written for a command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PromptState {
    #[default]
    Normal,
    Guidance,
    PatternEditing,
}

/// What a command row remembers: the rung the request marks as its default
/// until the user moves it, then the rung they chose, `None` being this time
/// only. A chosen rung is kept by its option id, so a late update that
/// reorders the ladder cannot change what the row means.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum RowChoice {
    #[default]
    Default,
    Chosen(Option<PermissionRowGrant>),
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

/// The request in front, borrowed from the queue alone so the rest of the
/// prompt stays free to change while it is read.
fn front_request(requests: &VecDeque<QueuedPermission>) -> Option<&PermissionRequest> {
    requests.front().map(|queued| queued.request.as_ref())
}

/// Everything a press or a click can land on. A list item is a rung on a
/// step-through page, a choice on Review, or a scope in Customize.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PromptTarget {
    Choice(Choice),
    Row(usize),
    Item(usize),
    Tab(usize),
    Review,
    Effect(Effect),
    Remember(PermissionLifetime),
    Hint(KeyEvent),
    Inspector(InspectorControl),
    VisualScope(ScopeControl),
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

pub struct PermissionPrompt {
    requests: VecDeque<QueuedPermission>,
    request_ids: HashSet<String>,
    state: PromptState,
    panel: Panel,
    /// Where `?` returns to from Details.
    before_details: Panel,
    /// The guidance, pattern, or phrase being typed.
    field: TextField,
    /// What the field last copied or cut, until the host puts it on the
    /// clipboard.
    copied: Option<String>,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    rows: Vec<RowChoice>,
    /// The rung chosen on a request without command rows, `None` while it is
    /// the request's default.
    authority: Option<String>,
    focus_row: Option<usize>,
    highlight: Choice,
    pending: Option<Pending>,
    step: Option<StepThrough>,
    customize: Option<Customize>,
    inspector: Option<PatternInspector>,
    scope_view: ScopeView,
    hits: Vec<PromptHit>,
    mouse_down: Option<PromptTarget>,
    hover: Option<PromptTarget>,
    focus: Option<PromptTarget>,
    /// Keeps the highlighted line in sight on the next draw.
    reveal: bool,
    area: Rect,
    awaiting_review: bool,
    input_freshness: InputFreshness,
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
            panel: Panel::Main,
            before_details: Panel::Main,
            field: TextField::new(FieldKind::Line),
            copied: None,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            rows: Vec::new(),
            authority: None,
            focus_row: None,
            highlight: Choice::Once,
            pending: None,
            step: None,
            customize: None,
            inspector: None,
            scope_view: ScopeView::default(),
            hits: Vec::new(),
            mouse_down: None,
            hover: None,
            focus: None,
            reveal: false,
            area: Rect::default(),
            awaiting_review: false,
            input_freshness: InputFreshness::default(),
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

    /// Takes a newer version of a queued request. The prompt in front keeps
    /// what the user chose, its panel, and anything typed; a change to what
    /// an answer would grant, or a new note, re-arms the input barrier so a
    /// press made before the redraw cannot land on it.
    pub fn update(&mut self, request: Box<PermissionRequest>) -> bool {
        let Some(index) = self
            .requests
            .iter()
            .position(|queued| queued.request.id == request.id)
        else {
            return false;
        };
        if index > 0 {
            self.requests[index].request = request;
            return true;
        }
        let before = self.answer_fingerprint();
        self.requests[0].request = request;
        self.reconcile();
        self.recheck_inspector();
        if self.answer_fingerprint() != before {
            self.input_freshness.barrier();
        }
        self.invalidate_controls();
        true
    }

    #[cfg(test)]
    pub(crate) fn open(
        &mut self,
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        subagent_id: Option<String>,
    ) {
        self.enqueue(
            Box::new(PermissionRequest::from_legacy(
                id,
                tool,
                scopes,
                serde_json::Value::Null,
                std::path::Path::new("/project"),
                true,
            )),
            subagent_id,
        );
    }

    pub(crate) fn tool(&self) -> Option<&ToolKey> {
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
        self.requester_name()
    }

    fn requester_name(&self) -> Option<&str> {
        self.requests
            .front()
            .and_then(|queued| queued.requester.as_deref())
    }

    pub fn resolve(&mut self, request_id: &str) -> bool {
        if self.request_id() != Some(request_id) {
            return false;
        }
        if let Some(request) = self.requests.pop_front() {
            self.request_ids.remove(&request.request.id);
        }
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
        front_request(&self.requests)
    }

    fn decision(&self, answer: PermissionAnswer) -> Option<PermissionDecision> {
        Some(PermissionDecision {
            request_id: self.request_id()?.to_owned(),
            answer,
        })
    }

    fn reset_view(&mut self) {
        self.input_freshness.barrier();
        self.state = PromptState::Normal;
        self.panel = Panel::Main;
        self.before_details = Panel::Main;
        self.rows = self.current().map_or_else(Vec::new, |request| {
            vec![RowChoice::Default; request.resources.len()]
        });
        self.authority = None;
        self.highlight = Choice::Once;
        self.pending = None;
        self.step = None;
        self.customize = None;
        self.inspector = None;
        self.scope_view = ScopeView::default();
        self.field.clear();
        self.scroll.reset();
        self.focus_row = self.first_new_row();
        self.focus = None;
        self.invalidate_controls();
    }

    /// Forgets where everything was drawn, so nothing can be pressed until
    /// the prompt is drawn again.
    fn invalidate_controls(&mut self) {
        self.awaiting_review = true;
        self.hits.clear();
        self.mouse_down = None;
        self.hover = None;
        self.scrollbar = Scrollbar::default();
    }

    /// Fits what the user chose to a newer version of the request: a chosen
    /// rung the update withdrew falls back to the row's default, and a
    /// confirmation for a scope that changed is dropped.
    fn reconcile(&mut self) {
        let project = self.project_available();
        let Some(request) = front_request(&self.requests) else {
            return;
        };
        let count = request.resources.len();
        let rows: Vec<_> = (0..count)
            .map(|row| match self.rows.get(row) {
                Some(RowChoice::Chosen(Some(grant))) if !decision::offers(request, row, grant) => {
                    RowChoice::Default
                }
                Some(choice) => choice.clone(),
                None => RowChoice::Default,
            })
            .collect();
        let authority = self.authority.take().filter(|id| {
            decision::main_ladder(request)
                .iter()
                .any(|option| option.id == *id)
        });
        let step = self
            .step
            .take()
            .map(|step| step.reconciled(request, project));
        let pending_answer = self.pending.as_ref().map(|pending| pending.answer.clone());
        self.rows = rows;
        self.authority = authority;
        self.step = step;
        if self
            .focus_row
            .is_none_or(|row| row >= count || !self.is_new_row(row))
        {
            self.focus_row = self.first_new_row();
        }
        if let Some(customize) = self.customize.take() {
            self.customize = Some(customize.clamped(self));
        }
        if pending_answer.is_some() && self.pending_answer_now() != pending_answer {
            self.pending = None;
            self.field.clear();
        }
        if !self.choices().contains(&self.highlight) {
            self.highlight = Choice::Once;
        }
    }

    /// What a press would now grant and every note it would be made under,
    /// so an update that changes either can be told apart from one that does
    /// not.
    fn answer_fingerprint(&self) -> (Vec<Option<PermissionAnswer>>, Vec<String>) {
        let answers = [Choice::Conversation, Choice::Project]
            .into_iter()
            .map(|choice| self.choice_answer(choice))
            .chain([
                self.step
                    .as_ref()
                    .zip(self.current())
                    .map(|(step, request)| step.answer(request)),
                self.customize
                    .as_ref()
                    .and_then(|customize| customize.answer(self)),
            ])
            .collect();
        let notes = self.notes().into_iter().map(|note| note.text).collect();
        (answers, notes)
    }

    /// The answer the action that opened the pending confirmation would give
    /// now.
    fn pending_answer_now(&self) -> Option<PermissionAnswer> {
        let pending = self.pending.as_ref()?;
        match self.panel {
            Panel::StepThrough => Some(self.step.as_ref()?.answer(self.current()?)),
            Panel::Customize => self.customize.as_ref()?.answer(self),
            _ => self.choice_answer(pending.choice?),
        }
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
