use caudra_workbench::text_field::{TextCommand, decode};
use crossterm::event::KeyEventKind;

use super::details::sensitive_text;
use super::{
    FieldKind, HINT_ENTER, HINT_ESC, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind, Overlay, Panel, PermissionAnswer, PermissionDecision, PermissionLifetime,
    PermissionPrompt, Position, PromptMouse, PromptState, PromptTarget, ScrollbarMouse, TextKey,
    command_ladders, grade_command_pattern, is_ctrl,
};

#[derive(Default)]
pub(super) struct InputFreshness {
    last_press: Option<KeyCode>,
    blocked_key: Option<KeyCode>,
}

impl InputFreshness {
    pub(super) fn barrier(&mut self) {
        self.blocked_key = self.blocked_key.or(self.last_press);
    }

    fn press(&mut self, key: KeyEvent) -> bool {
        self.last_press = Some(key.code);
        if self.blocked_key == Some(key.code) {
            false
        } else {
            self.blocked_key = None;
            true
        }
    }

    fn release(&mut self, key: KeyEvent) {
        if self.blocked_key == Some(key.code) {
            self.blocked_key = None;
        }
        if self.last_press == Some(key.code) {
            self.last_press = None;
        }
    }
}

pub(super) fn hint_key(label: &str) -> Option<KeyEvent> {
    let code = match label {
        HINT_ESC => KeyCode::Esc,
        "←" => KeyCode::Left,
        "→" => KeyCode::Right,
        "↑" => KeyCode::Up,
        "↓" => KeyCode::Down,
        "PgDn" => KeyCode::PageDown,
        "PgUp" => KeyCode::PageUp,
        "F2" => KeyCode::F(2),
        _ => match label.split('/').next()? {
            HINT_ENTER => KeyCode::Enter,
            first => {
                let mut chars = first.chars();
                let single = chars.next().filter(|c| c.is_ascii_graphic())?;
                chars.next().is_none().then_some(KeyCode::Char(single))?
            }
        },
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

impl PermissionPrompt {
    pub(super) fn decision_needs_rearm(&self) -> bool {
        let Some(mut key) = self.input_freshness.blocked_key else {
            return false;
        };
        if key == KeyCode::Enter {
            match &self.focus {
                Some(PromptTarget::Hint(focused)) => key = focused.code,
                Some(_) => return false,
                None => {}
            }
        }
        if self.panel == Panel::Details {
            return self.confirmation.is_none()
                && match key {
                    KeyCode::Char('d') => self.project_available(),
                    KeyCode::Char('D') => true,
                    _ => false,
                };
        }
        if let Some(inspector) = &self.inspector {
            return !inspector.is_editing() && key == KeyCode::Char('y');
        }
        if let Some(confirmation) = &self.confirmation {
            return confirmation.complete
                && (key == KeyCode::Enter
                    || key == KeyCode::Char('y') && confirmation.phrase.is_none());
        }
        match self.state {
            PromptState::DenyEditing => key == KeyCode::Enter,
            PromptState::PatternEditing => false,
            _ => match (&self.panel, key) {
                (Panel::Main, KeyCode::Esc | KeyCode::Char('y' | 'g' | 'n')) => true,
                (Panel::Main, KeyCode::Char('s')) => {
                    self.grants_lifetime(&PermissionLifetime::Conversation)
                }
                (Panel::Main, KeyCode::Char('a')) => {
                    self.grants_lifetime(&PermissionLifetime::Project)
                }
                (Panel::Scopes, KeyCode::Char('A')) => {
                    self.grants_lifetime(&PermissionLifetime::Global)
                }
                (Panel::Scopes, KeyCode::Char('d')) => self.project_available(),
                (Panel::Scopes, KeyCode::Char('D')) => true,
                _ => false,
            },
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        match key.kind {
            KeyEventKind::Release => {
                self.input_freshness.release(key);
                return None;
            }
            KeyEventKind::Repeat => {
                self.input_freshness.last_press = Some(key.code);
                self.input_freshness.barrier();
                if self.is_open() {
                    self.handle_repeat(key);
                }
                return None;
            }
            KeyEventKind::Press => {}
        }
        let fresh = self.input_freshness.press(key);
        if !self.is_open() {
            return None;
        }
        if !fresh {
            self.handle_repeat(key);
            return None;
        }
        let unseen_activation = self.awaiting_review
            && self.panel != Panel::Details
            && !matches!(
                self.state,
                PromptState::DenyEditing | PromptState::PatternEditing
            )
            && !self
                .inspector
                .as_ref()
                .is_some_and(|inspector| inspector.is_editing())
            && matches!(
                key.code,
                KeyCode::Char('y' | 's' | 'a' | 'A') | KeyCode::Enter
            );
        let decision = self.handle_press(key);
        if decision.is_some() || unseen_activation {
            self.input_freshness.barrier();
        }
        if decision.is_some() {
            self.invalidate_controls();
        }
        decision
    }

    fn handle_repeat(&mut self, key: KeyEvent) {
        let edits = decode(key, FieldKind::Line).is_some_and(TextCommand::repeats);
        if let Some(inspector) = &self.inspector
            && self.panel != Panel::Details
        {
            let allowed = if inspector.is_editing() {
                edits
            } else {
                matches!(
                    key.code,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Tab
                        | KeyCode::BackTab
                )
            };
            if allowed {
                self.handle_inspector_key(key);
            }
            return;
        }
        if (self.field_focused() && edits)
            || matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Tab
                    | KeyCode::BackTab
            )
        {
            self.handle_press(key);
        }
    }

    /// Whether typing lands in the text field: guidance, a prefix or a phrase
    /// being typed, or an inspector field being edited.
    fn field_focused(&self) -> bool {
        match &self.inspector {
            Some(inspector) if self.panel != Panel::Details => inspector.is_editing(),
            _ => {
                matches!(
                    self.state,
                    PromptState::DenyEditing | PromptState::PatternEditing
                ) || self.phrase_focused()
            }
        }
    }

    /// Whether a confirmation is waiting on its phrase being typed.
    fn phrase_focused(&self) -> bool {
        self.confirmation.as_ref().is_some_and(|confirmation| {
            confirmation.phrase.is_some()
                && confirmation.complete
                && !self.awaiting_review
                && self.panel != Panel::Details
        })
    }

    /// Hands `key` to the text field, keeping whatever it copies or cuts for
    /// the host's clipboard. False when the key is not the field's.
    pub(super) fn edit_field(&mut self, key: KeyEvent) -> bool {
        match self.field.handle_key(key) {
            TextKey::Ignored => false,
            TextKey::Copy(text) | TextKey::Cut(text) => {
                self.copied = Some(text);
                true
            }
            _ => true,
        }
    }

    pub(crate) fn take_copied(&mut self) -> Option<String> {
        self.copied.take()
    }

    fn handle_press(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        let request_id = self.request_id()?.to_owned();
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            // A focused field copies its selection; only with nothing to copy
            // does the chord deny.
            if self.field_focused() && self.edit_field(key) {
                return None;
            }
            return Some(PermissionDecision {
                request_id,
                answer: PermissionAnswer::Deny,
            });
        }
        if self.inspector.is_some() && self.panel != Panel::Details {
            return self.handle_inspector_key(key);
        }
        if matches!(
            self.state,
            PromptState::DenyEditing | PromptState::PatternEditing
        ) {
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                self.move_focus(
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                );
                return None;
            }
            if key.code == KeyCode::Enter
                && let Some(PromptTarget::Hint(focused)) = self.focus
                && focused.code != KeyCode::Enter
            {
                self.focus = None;
                return self.handle_press(focused);
            }
        }
        if self.state == PromptState::DenyEditing {
            return match key.code {
                KeyCode::Enter => {
                    let text = self.field.text().trim().to_string();
                    Some(PermissionDecision {
                        request_id,
                        answer: if text.is_empty() {
                            PermissionAnswer::Deny
                        } else {
                            PermissionAnswer::DenyWithGuidance(text)
                        },
                    })
                }
                KeyCode::Esc => {
                    self.leave_editor();
                    None
                }
                _ => {
                    self.edit_field(key);
                    None
                }
            };
        }
        if self.state == PromptState::PatternEditing {
            match key.code {
                KeyCode::Enter => self.commit_written_pattern(),
                KeyCode::Esc => self.leave_editor(),
                _ => {
                    self.edit_field(key);
                }
            }
            return None;
        }
        if key.code == KeyCode::Esc {
            if self.panel != Panel::Main {
                self.panel = Panel::Main;
            } else if self.confirmation.take().is_some() {
                self.state = PromptState::Normal;
                self.field.clear();
            } else {
                return Some(PermissionDecision {
                    request_id,
                    answer: PermissionAnswer::Deny,
                });
            }
            self.scroll.reset();
            self.invalidate_controls();
            return None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if self.phrase_focused() {
                // A phrase being typed keeps bare Home and End for its caret,
                // so Ctrl takes the review to either end instead.
                match key.code {
                    KeyCode::Home | KeyCode::End => {
                        self.scroll
                            .handle_key(KeyEvent::new(key.code, KeyModifiers::NONE));
                    }
                    _ => {
                        self.edit_field(key);
                    }
                }
            }
            return None;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.move_focus(
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
            );
            return None;
        }
        if key.code == KeyCode::Enter
            && let Some(target) = self.focus.clone()
            && target != PromptTarget::Hint(hint_key(HINT_ENTER)?)
        {
            return self.activate(target);
        }
        if key.code == KeyCode::F(2)
            || (key.code == KeyCode::Char('v') && self.confirmation_phrase().is_none())
        {
            self.panel = if self.panel == Panel::Details {
                Panel::Main
            } else {
                Panel::Details
            };
            self.scroll.reset();
            self.invalidate_controls();
            return None;
        }
        if self.panel == Panel::Details {
            if self.confirmation.is_none() {
                match key.code {
                    KeyCode::Char('d') => {
                        self.open_confirmation(PromptState::ConfirmDenyAlwaysLocal)
                    }
                    KeyCode::Char('D') => {
                        self.open_confirmation(PromptState::ConfirmDenyAlwaysGlobal)
                    }
                    _ => {}
                }
            }
            self.scroll.handle_key(key);
            return None;
        }
        if let Some(confirmation) = &self.confirmation {
            let scrolls = match key.code {
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::Up | KeyCode::Down => true,
                KeyCode::Home | KeyCode::End => !self.phrase_focused(),
                _ => false,
            };
            if scrolls {
                self.scroll.handle_key(key);
                return None;
            }
            if self.awaiting_review || !confirmation.complete {
                return None;
            }
            if let Some(phrase) = &confirmation.phrase {
                if key.code == KeyCode::Enter && self.field.text().trim() == phrase {
                    return Some(PermissionDecision {
                        request_id,
                        answer: confirmation.answer.clone(),
                    });
                }
                self.edit_field(key);
                return None;
            }
            return matches!(key.code, KeyCode::Char('y') | KeyCode::Enter).then(|| {
                PermissionDecision {
                    request_id,
                    answer: confirmation.answer.clone(),
                }
            });
        }
        if key.code == KeyCode::Char('r') || key.code == KeyCode::Char('?') {
            self.open_scope_editor();
            return None;
        }
        if key.code == KeyCode::Char('c') && self.covered_count() > 0 {
            self.expanded_covered = !self.expanded_covered;
            self.invalidate_controls();
            return None;
        }
        if self.steer(key.code) {
            return None;
        }
        if self.scroll.handle_key(key) {
            return None;
        }
        if self.panel == Panel::Scopes {
            match key.code {
                KeyCode::Char('p') => {
                    self.panel = Panel::Main;
                    self.scroll.reset();
                    self.invalidate_controls();
                }
                KeyCode::Char('e') => {
                    self.open_pattern_editor();
                }
                KeyCode::Char('i') => self.open_inspector(),
                KeyCode::Char('A') => return self.approve(PermissionLifetime::Global),
                KeyCode::Char('d') => self.open_confirmation(PromptState::ConfirmDenyAlwaysLocal),
                KeyCode::Char('D') => self.open_confirmation(PromptState::ConfirmDenyAlwaysGlobal),
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char('y') if !self.awaiting_review => Some(PermissionDecision {
                request_id,
                answer: PermissionAnswer::AllowOnce,
            }),
            KeyCode::Char('s') => self.approve(PermissionLifetime::Conversation),
            KeyCode::Char('a') => self.approve(PermissionLifetime::Project),
            KeyCode::Char('g' | 'n') => {
                self.state = PromptState::DenyEditing;
                self.field.clear();
                self.invalidate_controls();
                None
            }
            _ => None,
        }
    }

    fn leave_editor(&mut self) {
        self.state = PromptState::Normal;
        self.field.clear();
        self.invalidate_controls();
    }

    pub(super) fn invalidate_controls(&mut self) {
        self.awaiting_review = true;
        self.row_hits.clear();
        self.mouse_down = None;
        self.hover = None;
        self.focus = None;
        self.pending_reveal = None;
        self.scrollbar = Default::default();
    }

    pub(super) fn move_focus(&mut self, reverse: bool) {
        let mut controls = Vec::new();
        if self.panel != Panel::Details
            && let Some(panel) = self.inspector_panel()
        {
            controls.extend(
                panel
                    .fields()
                    .into_iter()
                    .map(|field| PromptTarget::Inspector(field.control)),
            );
        }
        for hit in &self.row_hits {
            if !controls.contains(&hit.target) {
                controls.push(hit.target.clone());
            }
        }
        if controls.is_empty() {
            return;
        }
        let index = self
            .focus
            .as_ref()
            .and_then(|focus| controls.iter().position(|target| target == focus));
        let next = match index {
            Some(index) if reverse => (index + controls.len() - 1) % controls.len(),
            Some(index) => (index + 1) % controls.len(),
            None if reverse => controls.len() - 1,
            None => 0,
        };
        self.focus = Some(controls[next].clone());
        if matches!(self.focus, Some(PromptTarget::Inspector(_))) {
            self.pending_reveal = self.focus.clone();
        }
    }

    pub(super) fn activate(&mut self, target: PromptTarget) -> Option<PermissionDecision> {
        match target {
            PromptTarget::VisualScope(authority, control) => {
                if self.confirmation.is_some() {
                    return None;
                }
                if self.scope_authority != authority {
                    self.scope_authority = authority;
                    self.scope_view = Default::default();
                }
                self.scope_view.activate(control);
                self.invalidate_controls();
                None
            }
            PromptTarget::Inspector(control) => {
                self.activate_inspector(control);
                None
            }
            PromptTarget::Scope => {
                if self.panel == Panel::Scopes {
                    self.panel = Panel::Main;
                    self.invalidate_controls();
                } else {
                    self.open_scope_editor();
                }
                None
            }
            PromptTarget::Authority(id) => {
                self.select_authority(id);
                self.open_scope_editor();
                None
            }
            PromptTarget::Hint(key) => {
                self.focus = (self.panel == Panel::Scopes
                    && matches!(
                        key.code,
                        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                    ))
                .then_some(PromptTarget::Scope);
                self.handle_press(key)
            }
        }
    }

    fn steer(&mut self, code: KeyCode) -> bool {
        let Some(target) = self.focus.clone() else {
            return false;
        };
        match target {
            PromptTarget::Authority(id) if matches!(code, KeyCode::Left | KeyCode::Right) => {
                self.select_authority(id);
                self.widen(code == KeyCode::Right);
            }
            PromptTarget::Scope => match code {
                KeyCode::Left | KeyCode::Right => self.widen(code == KeyCode::Right),
                KeyCode::Up | KeyCode::Down => self.move_selection(code == KeyCode::Up),
                _ => return false,
            },
            _ => return false,
        }
        true
    }

    pub(super) fn open_pattern_editor(&mut self) -> bool {
        let Some(row) = self.command_row() else {
            return false;
        };
        let Some(request) = self.current() else {
            return false;
        };
        let seed = self.scopes[row]
            .written
            .clone()
            .or_else(|| {
                command_ladders(request)[row]
                    .get(1)
                    .and_then(|rung| rung.group.as_ref())
                    .map(|group| group.value.clone())
            })
            .or_else(|| {
                request
                    .resources
                    .get(row)
                    .map(|resource| resource.value.clone())
            })
            .unwrap_or_default();
        let seed = if request
            .resources
            .get(row)
            .is_some_and(|resource| sensitive_text(&resource.value))
            || sensitive_text(&seed)
        {
            String::new()
        } else {
            seed
        };
        self.field.set_text(&seed);
        self.state = PromptState::PatternEditing;
        self.invalidate_controls();
        true
    }

    fn commit_written_pattern(&mut self) {
        let Some(row) = self.command_row() else {
            return;
        };
        let pattern = self.field.text().trim().to_owned();
        let usable = self
            .current()
            .and_then(|request| request.resources.get(row))
            .is_some_and(|resource| grade_command_pattern(&pattern, &resource.value).is_ok());
        if !usable {
            return;
        }
        let offered = self
            .current()
            .map_or(0, |request| command_ladders(request)[row].len());
        self.scopes[row].written = Some(pattern);
        self.scopes[row].rung = offered + 1;
        self.leave_editor();
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if let Some(inspector) = &self.inspector {
            if !inspector.is_editing() || self.panel == Panel::Details {
                return false;
            }
            self.field.paste(text);
            self.input_freshness = InputFreshness::default();
            self.refresh_inspector(None);
            return true;
        }
        let editing = matches!(
            self.state,
            PromptState::DenyEditing | PromptState::PatternEditing
        );
        if (!editing
            && (self.confirmation_phrase().is_none()
                || self.panel == Panel::Details
                || self.awaiting_review))
            || !self.is_open()
        {
            return false;
        }
        self.field.paste(text);
        self.input_freshness = InputFreshness::default();
        true
    }

    pub fn clear_hover(&mut self) {
        self.hover = None;
    }
    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
        self.row_hits.clear();
        self.mouse_down = None;
    }
    pub fn contains(&self, pos: Position) -> bool {
        self.area.contains(pos)
    }
    pub(super) fn target_at(&self, pos: Position) -> Option<&PromptTarget> {
        self.row_hits
            .iter()
            .find(|hit| hit.area.contains(pos))
            .map(|hit| &hit.target)
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> PromptMouse {
        if !self.is_open() {
            return PromptMouse::Passthrough;
        }
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return PromptMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                self.row_hits.clear();
                self.mouse_down = None;
                return PromptMouse::Consumed;
            }
        }
        let pos = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(target) = self.target_at(pos).cloned() else {
                    return PromptMouse::Passthrough;
                };
                self.mouse_down = Some(target);
                PromptMouse::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed) = self.mouse_down.take() else {
                    return PromptMouse::Passthrough;
                };
                if self.target_at(pos) != Some(&pressed) {
                    return PromptMouse::Consumed;
                }
                self.focus = None;
                let decision = self.activate(pressed);
                if let Some(decision) = decision {
                    self.input_freshness.barrier();
                    self.invalidate_controls();
                    PromptMouse::Decided(decision)
                } else {
                    PromptMouse::Consumed
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.mouse_down.is_some() => {
                PromptMouse::Consumed
            }
            MouseEventKind::Moved => {
                self.hover = self.target_at(pos).cloned();
                if self.hover.is_some() {
                    PromptMouse::Consumed
                } else {
                    PromptMouse::Passthrough
                }
            }
            _ => PromptMouse::Passthrough,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use serde_json::json;
    use test_case::test_case;

    use super::super::view::REARM_MESSAGE;
    use super::super::view::tests::{ROOMY_HEIGHT, ROOMY_WIDTH, key, open_prompt, render, request};
    use super::super::{
        Panel, PermissionAnswer, PermissionLifetime, PermissionPrompt, PromptMouse, PromptState,
        PromptTarget, Rect,
    };
    use super::hint_key;

    const GUIDANCE: &str = "Use a native read tool instead";
    const SECRET: &str = "private-test-value";
    const SENSITIVE_COMMAND: &str = "env API_TOKEN=private-test-value rm -rf /project";
    const REPEAT_COUNT: usize = 10;
    const DELAYED_FIRST_REPEAT: Duration = Duration::from_millis(600);

    fn hit(prompt: &PermissionPrompt, target: &PromptTarget) -> Rect {
        prompt
            .row_hits
            .iter()
            .find(|hit| hit.target == *target)
            .unwrap()
            .area
    }

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(prompt: &mut PermissionPrompt, target: PromptTarget) -> PromptMouse {
        let area = hit(prompt, &target);
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area))
    }

    #[test_case('y'; "once")]
    #[test_case('s'; "conversation")]
    #[test_case('a'; "project")]
    fn queued_decisions_need_a_fresh_frame_and_a_fresh_press(shortcut: char) {
        let mut prompt = open_prompt();
        prompt.enqueue(request("next", json!({"command": "cargo test"})), None);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let area = hit(&prompt, &PromptTarget::Hint(key(KeyCode::Char(shortcut))));
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        assert!(prompt.resolve("id"));
        assert!(prompt.handle_key(key(KeyCode::Char(shortcut))).is_none());
        assert!(render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(REARM_MESSAGE));
        assert!(!matches!(
            prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area)),
            PromptMouse::Decided(_)
        ));
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            let mut event = key(KeyCode::Char(shortcut));
            event.kind = kind;
            assert!(prompt.handle_key(event).is_none());
        }
        assert_eq!(
            prompt
                .handle_key(key(KeyCode::Char(shortcut)))
                .unwrap()
                .request_id,
            "next"
        );
    }

    #[test_case('y'; "once")]
    #[test_case('s'; "conversation")]
    #[test_case('a'; "project")]
    fn legacy_repeat_presses_require_an_explicit_rearming_gesture(shortcut: char) {
        let event = key(KeyCode::Char(shortcut));
        let mut prompt = open_prompt();
        prompt.enqueue(request("next", json!({"command": "cargo test"})), None);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert_eq!(prompt.handle_key(event).unwrap().request_id, "id");
        assert!(prompt.resolve("id"));
        assert!(render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(REARM_MESSAGE));
        for _ in 0..REPEAT_COUNT {
            assert!(prompt.handle_key(event).is_none());
            assert!(render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(REARM_MESSAGE));
            assert_eq!(prompt.request_id(), Some("next"));
        }
        assert!(prompt.handle_key(key(KeyCode::Tab)).is_none());
        assert!(!render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(REARM_MESSAGE));
        assert_eq!(prompt.handle_key(event).unwrap().request_id, "next");
        assert!(prompt.confirmation.is_none());
    }

    #[test]
    fn legacy_first_repeat_after_500ms_of_silence_cannot_approve_the_next_request() {
        let mut prompt = open_prompt();
        prompt.enqueue(request("next", json!({"command": "cargo test"})), None);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let event = key(KeyCode::Char('y'));
        assert!(prompt.handle_key(event).is_some());
        assert!(prompt.resolve("id"));
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        thread::sleep(DELAYED_FIRST_REPEAT);
        assert!(prompt.handle_key(event).is_none());
        assert_eq!(prompt.request_id(), Some("next"));
        prompt.handle_key(key(KeyCode::Tab));
        assert_eq!(prompt.handle_key(event).unwrap().request_id, "next");
    }

    #[test_case("release"; "release")]
    #[test_case("different_key"; "different_key")]
    #[test_case("mouse"; "mouse_press")]
    fn unambiguous_activation_rearms_the_next_request(activation: &str) {
        let mut prompt = open_prompt();
        prompt.enqueue(request("next", json!({"command": "cargo test"})), None);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
        prompt.resolve("id");
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let decision = match activation {
            "release" => {
                let mut release = key(KeyCode::Char('y'));
                release.kind = KeyEventKind::Release;
                assert!(prompt.handle_key(release).is_none());
                prompt.handle_key(key(KeyCode::Char('y'))).unwrap()
            }
            "different_key" => prompt.handle_key(key(KeyCode::Char('s'))).unwrap(),
            "mouse" => {
                let PromptMouse::Decided(decision) =
                    click(&mut prompt, PromptTarget::Hint(key(KeyCode::Char('y'))))
                else {
                    panic!("fresh mouse press did not decide");
                };
                decision
            }
            _ => unreachable!(),
        };
        assert_eq!(decision.request_id, "next");
    }

    #[test]
    fn navigation_repeats_and_unrelated_releases_do_not_rearm_an_approval() {
        let mut prompt = open_prompt();
        prompt.enqueue(request("next", json!({"command": "cargo test"})), None);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
        prompt.resolve("id");
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        for _ in 0..REPEAT_COUNT {
            let previous = prompt.focus.clone();
            let mut repeat = key(KeyCode::Tab);
            repeat.kind = KeyEventKind::Repeat;
            prompt.handle_key(repeat);
            assert_ne!(prompt.focus, previous);
            repeat.kind = KeyEventKind::Release;
            prompt.handle_key(repeat);
            assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        }
        prompt.handle_key(key(KeyCode::Tab));
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
    }

    #[test]
    fn unseen_press_is_not_made_fresh_by_rendering() {
        let mut prompt = open_prompt();
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        prompt.handle_key(key(KeyCode::Tab));
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
    }

    #[test]
    fn held_enter_cannot_accept_a_confirmation_opened_by_that_press() {
        let mut prompt = open_prompt();
        prompt
            .requests
            .front_mut()
            .unwrap()
            .request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_any_command")
            .unwrap()
            .confirmation = None;
        prompt.select_authority("allow_any_command".into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        prompt.focus = Some(PromptTarget::Hint(key(KeyCode::Char('s'))));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(prompt.confirmation.is_some());
        assert!(prompt.confirmation_phrase().is_none());
        assert!(render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(REARM_MESSAGE));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        let mut modified = key(KeyCode::Enter);
        modified.modifiers = KeyModifiers::SHIFT;
        assert!(prompt.handle_key(modified).is_none());
        prompt.handle_key(key(KeyCode::Tab));
        assert!(!render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(REARM_MESSAGE));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_some());
    }

    #[test_case(false, KeyCode::Enter, true; "phraseless_enter")]
    #[test_case(false, KeyCode::Char('y'), true; "phraseless_y")]
    #[test_case(false, KeyCode::Char('s'), false; "phraseless_previous_stage_key")]
    #[test_case(true, KeyCode::Enter, true; "phrase_enter")]
    #[test_case(true, KeyCode::Char('y'), false; "phrase_text_key")]
    #[test_case(true, KeyCode::Char('s'), false; "phrase_previous_stage_key")]
    fn confirmation_rearm_hint_only_describes_current_decision_keys(
        phrase: bool,
        blocked: KeyCode,
        shown: bool,
    ) {
        let mut prompt = open_prompt();
        if !phrase {
            prompt
                .requests
                .front_mut()
                .unwrap()
                .request
                .options
                .iter_mut()
                .find(|option| option.id == "allow_any_command")
                .unwrap()
                .confirmation = None;
        }
        prompt.select_authority("allow_any_command".into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        let frozen = prompt.confirmation.as_ref().unwrap().answer.clone();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let mut release = key(KeyCode::Char('s'));
        release.kind = KeyEventKind::Release;
        prompt.handle_key(release);
        let mut repeat = key(blocked);
        repeat.kind = KeyEventKind::Repeat;
        assert!(prompt.handle_key(repeat).is_none());
        assert_eq!(
            render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(REARM_MESSAGE),
            shown
        );
        assert!(prompt.handle_key(key(blocked)).is_none());
        assert_eq!(prompt.confirmation.as_ref().unwrap().answer, frozen);
    }

    #[test_case(false, "release"; "allow_after_release")]
    #[test_case(false, "key"; "allow_after_different_key")]
    #[test_case(false, "mouse"; "allow_after_fresh_click")]
    #[test_case(true, "release"; "deny_after_release")]
    #[test_case(true, "key"; "deny_after_different_key")]
    #[test_case(true, "mouse"; "deny_after_fresh_click")]
    fn mouse_opened_confirmation_blocks_a_previously_held_enter(deny: bool, rearm: &str) {
        let mut prompt = open_prompt();
        prompt
            .requests
            .front_mut()
            .unwrap()
            .request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_any_command")
            .unwrap()
            .confirmation = None;
        prompt.select_authority("allow_any_command".into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        if deny {
            prompt.handle_key(key(KeyCode::F(2)));
            render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        }
        assert!(prompt.focus.is_none());
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        let open = PromptTarget::Hint(key(KeyCode::Char(if deny { 'D' } else { 's' })));
        assert!(matches!(click(&mut prompt, open), PromptMouse::Consumed));
        let frozen = prompt.confirmation.as_ref().unwrap().answer.clone();
        assert!(prompt.confirmation_phrase().is_none());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        for _ in 0..REPEAT_COUNT {
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
            assert_eq!(prompt.confirmation.as_ref().unwrap().answer, frozen);
        }
        let mut unrelated_release = key(KeyCode::Char('x'));
        unrelated_release.kind = KeyEventKind::Release;
        prompt.handle_key(unrelated_release);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        let answer = if rearm == "mouse" {
            let PromptMouse::Decided(decision) =
                click(&mut prompt, PromptTarget::Hint(key(KeyCode::Enter)))
            else {
                panic!("fresh confirmation click did not decide")
            };
            decision.answer
        } else {
            if rearm == "release" {
                let mut release = key(KeyCode::Enter);
                release.kind = KeyEventKind::Release;
                prompt.handle_key(release);
            } else {
                prompt.handle_key(key(KeyCode::Tab));
            }
            render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer
        };
        assert_eq!(answer, frozen);
    }

    #[test_case(40, 10; "narrow_short")]
    #[test_case(80, 18; "normal")]
    #[test_case(140, 24; "wide")]
    fn mouse_approves_the_same_displayed_scope_as_keyboard(width: u16, height: u16) {
        for (shortcut, lifetime) in [
            ('y', PermissionLifetime::Once),
            ('s', PermissionLifetime::Conversation),
            ('a', PermissionLifetime::Project),
        ] {
            let mut prompt = open_prompt();
            let expected = if lifetime == PermissionLifetime::Once {
                PermissionAnswer::AllowOnce
            } else {
                prompt.allow_answer(lifetime)
            };
            render(&mut prompt, width, height);
            prompt.focus = Some(PromptTarget::Hint(key(KeyCode::Esc)));
            let PromptMouse::Decided(decision) = click(
                &mut prompt,
                PromptTarget::Hint(key(KeyCode::Char(shortcut))),
            ) else {
                panic!("visible approval did not decide");
            };
            assert_eq!(decision.answer, expected);
        }
    }

    #[test]
    fn tab_and_arrows_edit_scope_without_an_enter_approval_default() {
        let mut prompt = open_prompt();
        render(&mut prompt, 80, 18);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        prompt.handle_key(key(KeyCode::Tab));
        assert_eq!(prompt.focus, Some(PromptTarget::Scope));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(prompt.panel == Panel::Scopes);
        render(&mut prompt, 80, 18);
        let previous = prompt.scopes.clone();
        prompt.handle_key(key(KeyCode::Left));
        assert_ne!(prompt.scopes, previous);
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Right));
        assert_eq!(prompt.scopes, previous);
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Enter));
        assert!(prompt.panel == Panel::Main);
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::BackTab));
        assert_eq!(prompt.focus, Some(PromptTarget::Hint(key(KeyCode::Esc))));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            PermissionAnswer::Deny
        );
    }

    #[test]
    fn scope_button_and_arrow_clicks_match_keyboard_without_approving() {
        let mut mouse_prompt = open_prompt();
        let mut keyboard = open_prompt();
        render(&mut mouse_prompt, 80, 18);
        render(&mut keyboard, 80, 18);
        assert!(matches!(
            click(&mut mouse_prompt, PromptTarget::Scope),
            PromptMouse::Consumed
        ));
        keyboard.handle_key(key(KeyCode::Char('r')));
        for label in ["→", "↓", "↑", "←"] {
            render(&mut mouse_prompt, 80, 18);
            render(&mut keyboard, 80, 18);
            let event = hint_key(label).unwrap();
            assert!(matches!(
                click(&mut mouse_prompt, PromptTarget::Hint(event)),
                PromptMouse::Consumed
            ));
            assert!(keyboard.handle_key(event).is_none());
            assert_eq!(mouse_prompt.selected_option, keyboard.selected_option);
            assert_eq!(mouse_prompt.scopes, keyboard.scopes);
        }
        assert!(mouse_prompt.confirmation.is_none());
    }

    #[test_case('g'; "displayed_guidance")]
    #[test_case('n'; "guidance_alias")]
    fn guidance_and_escape_are_not_persistent_denials(shortcut: char) {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char(shortcut)));
        prompt.handle_paste(GUIDANCE);
        assert!(prompt.handle_key(key(KeyCode::Esc)).is_none());
        assert_eq!(prompt.state, PromptState::Normal);
        assert!(prompt.field.is_empty());
        prompt.handle_key(key(KeyCode::Char(shortcut)));
        prompt.handle_paste(GUIDANCE);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            PermissionAnswer::DenyWithGuidance(GUIDANCE.into())
        );
    }

    #[test_case("resize"; "resize")]
    #[test_case("update"; "coverage_refresh")]
    #[test_case("scroll"; "scroll")]
    fn changing_geometry_or_request_cancels_mouse_down(change: &str) {
        let mut prompt = open_prompt();
        render(&mut prompt, 80, 18);
        let area = hit(&prompt, &PromptTarget::Hint(key(KeyCode::Char('y'))));
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        match change {
            "resize" => {
                render(&mut prompt, 140, 18);
            }
            "update" => {
                prompt.update(request("id", json!({"command": "cargo test --tests"})));
            }
            "scroll" => prompt.scroll(1),
            _ => unreachable!(),
        }
        render(&mut prompt, 80, 18);
        assert!(!matches!(
            prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area)),
            PromptMouse::Decided(_)
        ));
    }

    #[test]
    fn refresh_clears_frozen_confirmation_and_pending_pattern_editor() {
        let mut prompt = open_prompt();
        prompt.select_authority("allow_any_command".into());
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Char('s')));
        assert!(prompt.confirmation.is_some());
        assert!(prompt.update(request("id", json!({"command": "cargo test --tests"}))));
        assert!(prompt.confirmation.is_none());
        assert_eq!(prompt.state, PromptState::Normal);
        assert!(prompt.handle_key(key(KeyCode::Char('a'))).is_none());
    }

    #[test]
    fn dangerous_custom_prefix_is_validated_and_requires_its_phrase() {
        let mut prompt = open_prompt();
        prompt.open_scope_editor();
        prompt.handle_key(key(KeyCode::Char('e')));
        prompt.field.clear();
        prompt.handle_paste("git *");
        prompt.handle_key(key(KeyCode::Enter));
        assert_eq!(prompt.state, PromptState::PatternEditing);
        prompt.field.clear();
        prompt.handle_paste("cargo *");
        prompt.handle_key(key(KeyCode::Enter));
        assert_eq!(prompt.state, PromptState::Normal);
        assert_eq!(prompt.scopes[0].written.as_deref(), Some("cargo *"));
        prompt.handle_key(key(KeyCode::Char('p')));
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Char('s')));
        let phrase = prompt.confirmation_phrase().unwrap().to_owned();
        render(&mut prompt, 40, 10);
        prompt.handle_paste(&phrase);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_some());
    }

    #[test]
    fn credential_bearing_command_is_not_seeded_into_the_prefix_editor() {
        let mut prompt = open_prompt();
        let request = &mut prompt.requests.front_mut().unwrap().request;
        request.input = json!({"command": SENSITIVE_COMMAND});
        request.resources[0].value = SENSITIVE_COMMAND.into();
        prompt.open_scope_editor();
        prompt.handle_key(key(KeyCode::Char('e')));
        assert!(prompt.field.is_empty());
        let screen = render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(!screen.contains(SECRET));
        assert!(screen.contains("rm -rf /project"));
        assert!(screen.contains("author a prefix explicitly"));
    }

    #[test]
    fn repeat_navigation_and_editing_never_confirm_or_open_an_editor() {
        let mut prompt = open_prompt();
        render(&mut prompt, 80, 18);
        let mut event = key(KeyCode::Char('g'));
        event.kind = KeyEventKind::Repeat;
        prompt.handle_key(event);
        assert_eq!(prompt.state, PromptState::Normal);
        event.kind = KeyEventKind::Release;
        prompt.handle_key(event);
        prompt.handle_key(key(KeyCode::Char('g')));
        prompt.handle_paste("abc");
        event.code = KeyCode::Backspace;
        event.kind = KeyEventKind::Repeat;
        prompt.handle_key(event);
        assert_eq!(prompt.field.text(), "ab");
        event.code = KeyCode::Enter;
        assert!(prompt.handle_key(event).is_none());
        assert_eq!(prompt.state, PromptState::DenyEditing);
    }

    #[test_case(false; "guidance")]
    #[test_case(true; "prefix")]
    fn enter_activates_the_focused_editor_back_control(prefix: bool) {
        let mut prompt = open_prompt();
        if prefix {
            prompt.open_scope_editor();
            prompt.handle_key(key(KeyCode::Char('e')));
        } else {
            prompt.handle_key(key(KeyCode::Char('g')));
        }
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::BackTab));
        assert_eq!(prompt.focus, Some(PromptTarget::Hint(key(KeyCode::Esc))));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(prompt.state, PromptState::Normal);
    }

    #[test]
    fn technical_details_cannot_type_into_a_hidden_confirmation() {
        let mut prompt = open_prompt();
        prompt.select_authority("allow_any_command".into());
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Char('s')));
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::F(2)));
        render(&mut prompt, 80, 18);
        let mut event = key(KeyCode::Char('x'));
        event.kind = KeyEventKind::Repeat;
        prompt.handle_key(event);
        assert!(!prompt.handle_paste("x"));
        assert!(prompt.field.is_empty());
    }

    #[test]
    fn ctrl_w_edits_the_guidance_instead_of_being_dropped() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('g')));
        prompt.handle_paste(GUIDANCE);
        let chord = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(prompt.handle_key(chord).is_none());
        assert_eq!(
            prompt.field.text(),
            GUIDANCE
                .rsplit_once(' ')
                .map_or("", |(head, _)| head)
                .to_owned()
                + " "
        );
        assert_eq!(prompt.state, PromptState::DenyEditing);
    }

    #[test_case(true; "selection_copies")]
    #[test_case(false; "no_selection_denies")]
    fn ctrl_c_copies_a_selected_field_before_denying(selected: bool) {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('g')));
        prompt.handle_paste(GUIDANCE);
        if selected {
            prompt.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        }
        let decision = prompt.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        match selected {
            true => {
                assert!(decision.is_none());
                assert_eq!(prompt.take_copied().as_deref(), Some(GUIDANCE));
                assert_eq!(prompt.state, PromptState::DenyEditing);
            }
            false => {
                assert_eq!(decision.unwrap().answer, PermissionAnswer::Deny);
                assert!(prompt.take_copied().is_none());
            }
        }
    }
}
