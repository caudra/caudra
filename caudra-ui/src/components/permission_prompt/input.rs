use caudra_agent::permissions::PermissionAnswer;
use caudra_workbench::text_field::{FieldKind, TextCommand, TextKey, decode};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;

use super::choices::Choice;
use super::customize::ScopeItem;
use super::step_through::{REVIEW_CHOICES, ReviewChoice};
use super::{Panel, PermissionDecision, PermissionPrompt, PromptMouse, PromptState, PromptTarget};
use crate::components::permission_scope::view::ScopeView;
use crate::components::scrollbar::ScrollbarMouse;
use crate::components::{Overlay, is_ctrl};

/// Holds back a key that was already down when what it would answer last
/// changed, until it is let go or another key is pressed, so a held or
/// repeated key cannot answer something the user has not seen.
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

fn backwards(key: &KeyEvent) -> bool {
    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT)
}

fn digit(code: KeyCode) -> Option<usize> {
    match code {
        KeyCode::Char(digit @ '1'..='9') => Some(digit as usize - '1' as usize),
        _ => None,
    }
}

fn review_index(choice: ReviewChoice) -> usize {
    REVIEW_CHOICES
        .iter()
        .position(|found| *found == choice)
        .unwrap_or_default()
}

impl PermissionPrompt {
    /// Whether the key being held back would answer, so the footer says how
    /// to get it through.
    pub(super) fn decision_needs_rearm(&self) -> bool {
        let Some(key) = self.input_freshness.blocked_key else {
            return false;
        };
        if self.inspector.is_some() && self.panel != Panel::Details {
            return false;
        }
        if self.pending.is_some() {
            return key == KeyCode::Enter;
        }
        match (self.state, self.panel) {
            (PromptState::Guidance, _) => key == KeyCode::Enter,
            (PromptState::PatternEditing, _) => false,
            (_, Panel::Details) => key == KeyCode::Esc,
            (_, Panel::Customize) => key == KeyCode::Enter,
            (_, Panel::StepThrough) => {
                self.step.as_ref().is_some_and(|step| step.page.is_none())
                    && (key == KeyCode::Enter
                        || key == KeyCode::Char('y')
                        || digit(key) == Some(review_index(ReviewChoice::Remember)))
            }
            (_, Panel::Main) => {
                matches!(key, KeyCode::Esc | KeyCode::Enter)
                    || self
                        .choice_for(key)
                        .is_some_and(|choice| choice != Choice::Deny)
            }
        }
    }

    /// The main view's choice a number or letter names, when it is on offer.
    fn choice_for(&self, code: KeyCode) -> Option<Choice> {
        let choice = match code {
            KeyCode::Char(letter) => match digit(code) {
                Some(index) => self.choices().get(index).copied(),
                None => Choice::from_letter(letter),
            },
            _ => None,
        }?;
        self.choices().contains(&choice).then_some(choice)
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
            && !self.field_focused()
            && (matches!(key.code, KeyCode::Enter | KeyCode::Char('y' | 's' | 'a'))
                || digit(key.code).is_some());
        let decision = self.handle_press(key);
        if decision.is_some() || unseen_activation {
            self.input_freshness.barrier();
        }
        if decision.is_some() {
            self.invalidate_controls();
        }
        decision
    }

    /// A held key may move and scroll, and edit a field, but never answer.
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
                    | KeyCode::Char('<' | '>' | 'j' | 'k')
            ) && !self.field_focused()
        {
            self.handle_press(key);
        }
    }

    /// Whether typing lands in the text field.
    fn field_focused(&self) -> bool {
        match &self.inspector {
            Some(inspector) if self.panel != Panel::Details => inspector.is_editing(),
            _ => {
                self.panel != Panel::Details
                    && (matches!(
                        self.state,
                        PromptState::Guidance | PromptState::PatternEditing
                    ) || self.phrase_focused())
            }
        }
    }

    /// Whether a grant waits on a phrase being typed.
    fn phrase_focused(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| !pending.phrases.is_empty())
            && !self.awaiting_review
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
        self.current()?;
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            if self.field_focused() && self.edit_field(key) {
                return None;
            }
            return self.decision(PermissionAnswer::Deny);
        }
        if self.inspector.is_some() && self.panel != Panel::Details {
            self.handle_inspector_key(key);
            return None;
        }
        if self.pending.is_some() && self.panel != Panel::Details {
            return self.handle_pending_key(key);
        }
        match self.state {
            PromptState::Guidance => return self.handle_guidance_key(key),
            PromptState::PatternEditing => {
                self.handle_pattern_key(key);
                return None;
            }
            PromptState::Normal => {}
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match self.panel {
            Panel::Main => self.handle_main_key(key),
            Panel::StepThrough => self.handle_step_key(key),
            Panel::Customize => self.handle_customize_key(key),
            Panel::Details => self.handle_details_key(key),
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        match key.code {
            KeyCode::Esc => self.decision(PermissionAnswer::Deny),
            KeyCode::Enter => self.choose(self.highlight),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_highlight(false);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_highlight(true);
                None
            }
            KeyCode::Left | KeyCode::Right => {
                self.widen_focused(key.code == KeyCode::Right);
                None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.cycle_row(backwards(&key));
                None
            }
            KeyCode::Char(bracket @ ('<' | '>')) => {
                self.widen_all(bracket == '>');
                None
            }
            KeyCode::Char('e') => {
                self.open_editor();
                None
            }
            KeyCode::Char('?') => {
                self.toggle_details();
                None
            }
            code @ KeyCode::Char(_) => self.choose(self.choice_for(code)?),
            _ => {
                self.scroll.handle_key(key);
                None
            }
        }
    }

    fn handle_pending_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        match key.code {
            KeyCode::Esc => {
                self.pending = None;
                self.field.clear();
                self.invalidate_controls();
                None
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.scroll.handle_key(key);
                None
            }
            KeyCode::Enter => {
                if self.awaiting_review {
                    return None;
                }
                let typed = self.field.text().trim().to_owned();
                let pending = self.pending.as_mut()?;
                if let Some(phrase) = pending.phrases.first() {
                    if typed != *phrase {
                        return None;
                    }
                    pending.phrases.remove(0);
                    self.field.clear();
                    if self
                        .pending
                        .as_ref()
                        .is_some_and(|pending| !pending.phrases.is_empty())
                    {
                        self.invalidate_controls();
                        return None;
                    }
                }
                let answer = self.pending.take()?.answer;
                self.decision(answer)
            }
            _ => {
                if self.phrase_focused() {
                    self.edit_field(key);
                }
                None
            }
        }
    }

    fn handle_guidance_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        match key.code {
            KeyCode::Enter => {
                let guidance = self.field.text().trim().to_owned();
                self.decision(if guidance.is_empty() {
                    PermissionAnswer::Deny
                } else {
                    PermissionAnswer::DenyWithGuidance(guidance)
                })
            }
            KeyCode::Esc => {
                self.leave_field();
                None
            }
            _ => {
                self.edit_field(key);
                None
            }
        }
    }

    fn handle_pattern_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                match self.panel {
                    Panel::StepThrough => self.commit_page_pattern(),
                    Panel::Customize => self.commit_customize_pattern(),
                    Panel::Main | Panel::Details => false,
                };
            }
            KeyCode::Esc => self.leave_field(),
            _ => {
                self.edit_field(key);
            }
        }
    }

    fn leave_field(&mut self) {
        self.state = PromptState::Normal;
        self.field.clear();
        self.invalidate_controls();
    }

    fn handle_step_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        let review = self.step.as_ref()?.page.is_none();
        match key.code {
            KeyCode::Esc => {
                self.discard_step_through();
                None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.turn_page(!backwards(&key));
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_step_highlight(false);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_step_highlight(true);
                None
            }
            KeyCode::Left | KeyCode::Right if !review => {
                self.step_lifetime(key.code == KeyCode::Right);
                None
            }
            KeyCode::Enter => {
                let index = self.step.as_ref()?.highlight;
                self.choose_step_item(index)
            }
            KeyCode::Char('y') if review => {
                self.choose_step_item(review_index(ReviewChoice::Remember))
            }
            KeyCode::Char('n') if review => self.choose_step_item(review_index(ReviewChoice::Deny)),
            KeyCode::Char('i') if !review => {
                self.open_inspector();
                None
            }
            KeyCode::Char('?') => {
                self.toggle_details();
                None
            }
            code => match digit(code) {
                Some(index) => self.choose_step_item(index),
                None => {
                    self.scroll.handle_key(key);
                    None
                }
            },
        }
    }

    fn move_step_highlight(&mut self, forward: bool) {
        let (Some(request), Some(step)) = (self.current(), &self.step) else {
            return;
        };
        let count = match step.page {
            Some(row) => step.items(request, row).len(),
            None => REVIEW_CHOICES.len(),
        };
        if let Some(step) = &mut self.step {
            step.highlight = if forward {
                (step.highlight + 1).min(count.saturating_sub(1))
            } else {
                step.highlight.saturating_sub(1)
            };
        }
        self.reveal = true;
    }

    fn handle_customize_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        match key.code {
            KeyCode::Esc => {
                self.close_customize();
                None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.cycle_customize_field(backwards(&key));
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_scope(false);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_scope(true);
                None
            }
            KeyCode::Left | KeyCode::Right => {
                self.step_customize(key.code == KeyCode::Right);
                None
            }
            KeyCode::Enter => self.apply_customize(),
            KeyCode::Char('v') => {
                self.toggle_advanced();
                None
            }
            KeyCode::Char('i') => {
                self.open_inspector();
                None
            }
            KeyCode::Char('?') => {
                self.toggle_details();
                None
            }
            _ => {
                self.scroll.handle_key(key);
                None
            }
        }
    }

    fn toggle_advanced(&mut self) {
        if self.customize_model().is_none() {
            return;
        }
        if let Some(customize) = self.customize.as_mut() {
            customize.advanced = !customize.advanced;
        }
        self.scope_view = ScopeView::default();
        self.reveal = true;
        self.invalidate_controls();
    }

    fn handle_details_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        match key.code {
            KeyCode::Esc => self.decision(PermissionAnswer::Deny),
            KeyCode::Char('?') => {
                self.toggle_details();
                None
            }
            _ => {
                self.scroll.handle_key(key);
                None
            }
        }
    }

    /// `?` opens Details over whatever is shown, and closes it again.
    pub(super) fn toggle_details(&mut self) {
        if self.panel == Panel::Details {
            self.panel = self.before_details;
        } else {
            self.before_details = self.panel;
            self.panel = Panel::Details;
        }
        self.scroll.reset();
        self.invalidate_controls();
    }

    /// `e`: the step-through when several commands need an answer, else
    /// Customize.
    fn open_editor(&mut self) {
        if self.batch() && self.per_row() && !self.new_rows().is_empty() {
            self.open_step_through();
        } else {
            self.open_customize(false);
        }
    }

    fn widen_focused(&mut self, forward: bool) {
        if self.opacity().is_some() {
            return;
        }
        let changed = if self.per_row() {
            let Some(row) = self.focus_row else {
                return;
            };
            self.widen_row(row, forward)
        } else {
            self.widen_authority(forward)
        };
        if changed {
            self.scope_changed();
        }
    }

    fn widen_all(&mut self, forward: bool) {
        if !self.per_row() || self.opacity().is_some() {
            return;
        }
        let mut changed = false;
        for row in self.new_rows() {
            changed |= self.widen_row(row, forward);
        }
        if changed {
            self.scope_changed();
        }
    }

    fn scope_changed(&mut self) {
        if !self.choices().contains(&self.highlight) {
            self.highlight = Choice::Once;
        }
        self.invalidate_controls();
    }

    fn cycle_row(&mut self, backwards: bool) {
        let rows = self.new_rows();
        if rows.is_empty() {
            return;
        }
        let index = self
            .focus_row
            .and_then(|row| rows.iter().position(|found| *found == row));
        let next = match index {
            Some(index) if backwards => (index + rows.len() - 1) % rows.len(),
            Some(index) => (index + 1) % rows.len(),
            None => 0,
        };
        self.focus_row = Some(rows[next]);
        self.invalidate_controls();
    }

    /// Acts on what was clicked, or on a focused control's Enter.
    pub(super) fn activate(&mut self, target: PromptTarget) -> Option<PermissionDecision> {
        match target {
            PromptTarget::Choice(choice) => {
                if let Some(pending) = &self.pending {
                    let again = pending.choice == Some(choice) && pending.phrases.is_empty();
                    return again
                        .then(|| self.handle_pending_key(KeyEvent::from(KeyCode::Enter)))
                        .flatten();
                }
                if self.state != PromptState::Normal {
                    return None;
                }
                self.choose(choice)
            }
            PromptTarget::Row(row) => {
                self.focus_row = Some(row);
                self.invalidate_controls();
                None
            }
            PromptTarget::Item(index) => match self.panel {
                Panel::StepThrough => {
                    if let Some(step) = self.step.as_mut() {
                        step.highlight = index;
                    }
                    self.choose_step_item(index)
                }
                Panel::Customize => {
                    let own = self.customize_items().get(index) == Some(&ScopeItem::OwnPattern);
                    self.highlight_scope(index);
                    if own {
                        return self.apply_customize();
                    }
                    None
                }
                Panel::Main | Panel::Details => None,
            },
            PromptTarget::Tab(row) => {
                self.go_to_page(Some(row));
                None
            }
            PromptTarget::Review => {
                self.go_to_page(None);
                None
            }
            PromptTarget::Effect(effect) => {
                self.set_effect(effect);
                None
            }
            PromptTarget::Remember(lifetime) => {
                match self.panel {
                    Panel::Customize => self.set_lifetime(lifetime),
                    Panel::StepThrough => self.set_page_lifetime(lifetime),
                    Panel::Main | Panel::Details => {}
                }
                None
            }
            PromptTarget::Hint(key) => self.handle_press(key),
            PromptTarget::Inspector(control) => {
                self.activate_inspector(control);
                None
            }
            PromptTarget::VisualScope(control) => {
                self.scope_view.activate(control);
                self.invalidate_controls();
                None
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.is_open() {
            return false;
        }
        if let Some(inspector) = &self.inspector
            && self.panel != Panel::Details
        {
            if !inspector.is_editing() {
                return false;
            }
            self.field.paste(text);
            self.input_freshness = InputFreshness::default();
            self.refresh_inspector(None);
            return true;
        }
        if !self.field_focused() {
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
        self.hits.clear();
        self.mouse_down = None;
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.area.contains(pos)
    }

    fn target_at(&self, pos: Position) -> Option<&PromptTarget> {
        self.hits
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
                self.scroll
                    .scroll_to(u16::try_from(top).unwrap_or(u16::MAX));
                self.hits.clear();
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
                match self.activate(pressed) {
                    Some(decision) => {
                        self.input_freshness.barrier();
                        self.invalidate_controls();
                        PromptMouse::Decided(decision)
                    }
                    None => PromptMouse::Consumed,
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
    use caudra_agent::permissions::{
        COMMAND_EXACT_PREFIX, CONFINED_READ_AUTHORITY, PermissionAnswer, PermissionLifetime,
        PermissionRequest, PermissionResourceAccess, PermissionRowGrant, RuleOrigin,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use test_case::test_case;

    use super::super::choices::Choice;
    use super::super::decision::tests::{commands_request, native_shell_request};
    use super::super::decision::{default_grant, grant_option};
    use super::super::step_through::PageItem;
    use super::super::view::tests::{
        BATCH, OUTSIDE_FILE, ROOMY_HEIGHT, ROOMY_WIDTH, SINGLE_COMMAND, advised, batch_request,
        cover, fetch_request, file_request, heredoc_request, key, planning, prompt_for, prose,
        render, shell_prompt,
    };
    use super::super::view::{PRESS_AGAIN, REARM_MESSAGE};
    use super::super::{
        Panel, PermissionPrompt, PromptMouse, PromptState, PromptTarget, RowChoice,
    };
    use super::ScopeItem;
    use crate::components::command_text::tests::coloured;

    const FIRST: &str = "first";
    const NEXT: &str = "next";
    const PAIR: [&str; 2] = [
        "cargo fmt -p caudra-agent",
        "cargo clippy -p caudra-agent --tests",
    ];
    const GUIDANCE: &str = "Use a native read tool instead";
    const SECRET: &str = "private-test-value";
    const SENSITIVE_COMMAND: &str = "env API_TOKEN=private-test-value rm -rf /project";
    const WRONG_PHRASE: &str = "not the phrase";
    const BROAD_PHRASE: &str = "ALLOW BROAD SHELL ACCESS";
    const GIT_LOG: &str = "git log -1";
    const RUSTFMT_CHECK: &str = "rustfmt --edition 2024 --check f.rs";
    const RUSTFMT_LADDER: [&str; 4] = [
        "this command",
        "rustfmt --edition 2024 --check *",
        "rustfmt --edition 2024 *",
        "rustfmt *",
    ];
    const UNMATCHED_PATTERN: &str = "git *";
    const MATCHING_PATTERN: &str = "cargo test -p *";
    const SHORTCUT_LETTERS: &str = "yn?e";
    const REPEAT_COUNT: usize = 10;

    fn hit(prompt: &PermissionPrompt, target: &PromptTarget) -> Rect {
        prompt
            .hits
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

    fn with_kind(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        let mut event = key(code);
        event.kind = kind;
        event
    }

    fn draw(prompt: &mut PermissionPrompt) -> String {
        render(prompt, ROOMY_WIDTH, ROOMY_HEIGHT)
    }

    /// Two shell requests queued one after the other.
    fn queued_pair() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        for id in [FIRST, NEXT] {
            let mut request = native_shell_request(SINGLE_COMMAND);
            request.id = id.into();
            prompt.enqueue(Box::new(request), None);
        }
        prompt
    }

    /// Customize applied on the broadest scope it lists, waiting on its
    /// confirmation. The Enter that applied it is still down.
    fn broad_pending(lifetime: PermissionLifetime) -> PermissionPrompt {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        prompt.open_customize(false);
        let last = prompt.customize_items().len() - 1;
        prompt.highlight_scope(last);
        prompt.set_lifetime(lifetime);
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(prompt.pending.is_some());
        draw(&mut prompt);
        prompt
    }

    fn decided(mouse: PromptMouse) -> Option<PermissionAnswer> {
        match mouse {
            PromptMouse::Decided(decision) => Some(decision.answer),
            _ => None,
        }
    }

    #[test_case('1', 'y', Choice::Once; "once")]
    #[test_case('2', 's', Choice::Conversation; "conversation")]
    #[test_case('3', 'a', Choice::Project; "project")]
    fn number_and_letter_keys_choose_the_same_answer(number: char, letter: char, choice: Choice) {
        let expected = shell_prompt(SINGLE_COMMAND).choice_answer(choice).unwrap();
        for shortcut in [Some(number), Some(letter), None] {
            let mut prompt = shell_prompt(SINGLE_COMMAND);
            draw(&mut prompt);
            let answer = match shortcut {
                Some(shortcut) => prompt
                    .handle_key(key(KeyCode::Char(shortcut)))
                    .map(|decision| decision.answer),
                None => decided(click(&mut prompt, PromptTarget::Choice(choice))),
            };
            assert_eq!(answer.as_ref(), Some(&expected), "{shortcut:?}");
        }
    }

    #[test]
    fn absent_choices_ignore_their_letters() {
        let mut prompt = prompt_for(heredoc_request());
        draw(&mut prompt);
        for letter in ['s', 'a'] {
            assert!(prompt.handle_key(key(KeyCode::Char(letter))).is_none());
            assert_eq!(prompt.state, PromptState::Normal);
            assert!(prompt.pending.is_none());
        }
        assert!(prompt.handle_key(key(KeyCode::Char('2'))).is_none());
        assert_eq!(prompt.state, PromptState::Guidance);
    }

    #[test]
    fn plan_mode_keeps_a_narrow_scope_for_the_project() {
        let mut prompt = prompt_for(planning(native_shell_request(SINGLE_COMMAND)));
        draw(&mut prompt);
        let answer = prompt.handle_key(key(KeyCode::Char('a'))).unwrap().answer;
        let PermissionAnswer::AllowComposed { rows } = answer else {
            panic!("expected a composed answer: {answer:?}");
        };
        assert_eq!(
            rows[0].as_ref().unwrap().lifetime,
            PermissionLifetime::Project
        );
    }

    #[test]
    fn plan_mode_customize_keeps_only_narrow_scopes_for_the_project() {
        let mut prompt = prompt_for(planning(native_shell_request(GIT_LOG)));
        draw(&mut prompt);
        prompt.open_customize(false);
        let items = prompt.customize_items();
        let narrow = default_grant(prompt.current().unwrap(), 0).map(ScopeItem::Row);
        let narrow = items.iter().position(|item| Some(item) == narrow.as_ref());
        let broad = items
            .iter()
            .position(|item| matches!(item, ScopeItem::Whole(_)));
        let mut keeps_for_the_project = |index: Option<usize>| {
            prompt.highlight_scope(index.unwrap());
            prompt
                .customize_lifetimes()
                .contains(&PermissionLifetime::Project)
        };
        assert!(keeps_for_the_project(narrow));
        assert!(!keeps_for_the_project(broad));
    }

    /// `→` walks a row out through every ancestor of its command to the bare
    /// executable, and `←` walks it back.
    #[test]
    fn arrows_walk_a_row_through_its_ancestors() {
        let mut prompt = prompt_for(native_shell_request(RUSTFMT_CHECK));
        draw(&mut prompt);
        let start = prompt.row_grants()[0].clone();
        let mut walked = vec![start.clone()];
        for _ in 1..RUSTFMT_LADDER.len() {
            assert!(prompt.handle_key(key(KeyCode::Right)).is_none());
            draw(&mut prompt);
            walked.push(prompt.row_grants()[0].clone());
        }
        let request = prompt.current().unwrap();
        let scopes: Vec<_> = walked
            .iter()
            .map(|grant| {
                grant_option(request, 0, grant.as_ref().unwrap())
                    .and_then(|option| option.group.as_ref())
                    .map(|group| group.value.as_str())
            })
            .collect();
        assert_eq!(scopes, RUSTFMT_LADDER.map(Some));
        for _ in 1..RUSTFMT_LADDER.len() {
            prompt.handle_key(key(KeyCode::Left));
            draw(&mut prompt);
        }
        assert_eq!(prompt.row_grants()[0], start);
    }

    #[test_case(native_shell_request(SINGLE_COMMAND), KeyCode::Left, KeyCode::Right; "command_row_narrows")]
    #[test_case(fetch_request(), KeyCode::Right, KeyCode::Left; "web_page_widens")]
    fn arrows_change_scope_without_answering(
        request: PermissionRequest,
        there: KeyCode,
        back: KeyCode,
    ) {
        let mut prompt = prompt_for(request);
        draw(&mut prompt);
        let answer = prompt.choice_answer(Choice::Conversation);
        let sentence = prompt.choice_sentence(Choice::Conversation);
        assert!(prompt.handle_key(key(there)).is_none());
        assert_ne!(prompt.choice_answer(Choice::Conversation), answer);
        assert_ne!(prompt.choice_sentence(Choice::Conversation), sentence);
        assert!(prompt.pending.is_none());
        draw(&mut prompt);
        assert!(prompt.handle_key(key(back)).is_none());
        assert_eq!(prompt.choice_answer(Choice::Conversation), answer);
    }

    #[test]
    fn angle_brackets_move_every_new_row() {
        let mut prompt = prompt_for(batch_request());
        draw(&mut prompt);
        let defaults = prompt.row_grants();
        let new_rows = prompt.new_rows();
        assert!(prompt.handle_key(key(KeyCode::Char('<'))).is_none());
        let narrowed = prompt.row_grants();
        for row in 0..BATCH.len() {
            assert_eq!(
                narrowed[row] != defaults[row],
                new_rows.contains(&row),
                "row {row}"
            );
        }
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Char('>'))).is_none());
        assert_eq!(prompt.row_grants(), defaults);
    }

    #[test]
    fn tab_cycles_only_new_rows() {
        let mut prompt = prompt_for(batch_request());
        draw(&mut prompt);
        let new_rows = prompt.new_rows();
        assert!(new_rows.len() > 1 && new_rows.len() < BATCH.len());
        assert_eq!(prompt.focus_row, new_rows.first().copied());
        let mut visited = Vec::new();
        for _ in 0..new_rows.len() {
            prompt.handle_key(key(KeyCode::Tab));
            visited.extend(prompt.focus_row);
        }
        assert_eq!(visited, [&new_rows[1..], &new_rows[..1]].concat());
        prompt.handle_key(key(KeyCode::BackTab));
        assert_eq!(prompt.focus_row, new_rows.last().copied());
    }

    /// The key that chooses item `index` of a page.
    fn number_key(index: usize) -> KeyEvent {
        key(KeyCode::Char(
            char::from_digit(index as u32 + 1, 10).unwrap(),
        ))
    }

    fn drafted_lifetime(prompt: &PermissionPrompt, row: usize) -> PermissionLifetime {
        prompt.step.as_ref().unwrap().lifetimes[row].clone()
    }

    #[test]
    fn step_through_escape_discards_the_draft() {
        let mut prompt = prompt_for(commands_request(&PAIR));
        draw(&mut prompt);
        let rows = prompt.row_grants();
        assert!(prompt.handle_key(key(KeyCode::Char('e'))).is_none());
        assert_eq!(prompt.panel, Panel::StepThrough);
        draw(&mut prompt);
        assert!(prompt.handle_key(number_key(0)).is_none());
        assert_eq!(
            prompt.step.as_ref().unwrap().rows[0],
            RowChoice::Chosen(None)
        );
        assert!(prompt.handle_key(key(KeyCode::Esc)).is_none());
        assert_eq!(prompt.panel, Panel::Main);
        assert!(prompt.step.is_none());
        assert_eq!(prompt.row_grants(), rows);
    }

    #[test_case(false, &[PermissionLifetime::Conversation, PermissionLifetime::Project]; "build_mode")]
    #[test_case(true, &[PermissionLifetime::Conversation]; "plan_mode_holds_a_broad_rung_to_the_conversation")]
    fn remember_for_offers_only_the_rungs_lifetimes(plan: bool, offered: &[PermissionLifetime]) {
        let mut request = commands_request(&PAIR);
        let exact = format!("{COMMAND_EXACT_PREFIX}0");
        let Some(PermissionRowGrant::Offered(default)) = default_grant(&request, 0) else {
            panic!("row 0 has no offered default");
        };
        assert_ne!(default, exact);
        for option in &mut request.options {
            if option.id == default {
                option.allowed_lifetimes = vec![
                    PermissionLifetime::Once,
                    PermissionLifetime::Conversation,
                    PermissionLifetime::Project,
                ];
                option.confirmation = Some(BROAD_PHRASE.into());
            } else if option.id == exact {
                option.allowed_lifetimes =
                    vec![PermissionLifetime::Once, PermissionLifetime::Conversation];
            }
        }
        let mut prompt = prompt_for(if plan { planning(request) } else { request });
        draw(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('e')));
        assert_eq!(prompt.page_lifetimes(), offered);
        for _ in 0..REPEAT_COUNT {
            prompt.handle_key(key(KeyCode::Right));
        }
        assert_eq!(drafted_lifetime(&prompt, 0), *offered.last().unwrap());

        let request = prompt.current().unwrap();
        let exact_item = PageItem::Grant(Some(PermissionRowGrant::Offered(exact)));
        let index = prompt
            .step
            .as_ref()
            .unwrap()
            .items(request, 0)
            .iter()
            .position(|item| *item == exact_item)
            .unwrap();
        draw(&mut prompt);
        prompt.handle_key(number_key(index));
        assert_eq!(
            drafted_lifetime(&prompt, 0),
            PermissionLifetime::Conversation
        );

        prompt.go_to_page(Some(0));
        draw(&mut prompt);
        prompt.handle_key(number_key(0));
        prompt.go_to_page(Some(0));
        assert!(prompt.page_lifetimes().is_empty());
    }

    #[test]
    fn review_answer_carries_each_rows_lifetime() {
        let mut prompt = prompt_for(commands_request(&PAIR));
        draw(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('e')));
        prompt.handle_key(key(KeyCode::Right));
        for _ in PAIR {
            draw(&mut prompt);
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        }
        assert_eq!(prompt.step.as_ref().unwrap().page, None);
        draw(&mut prompt);
        let answer = prompt.handle_key(key(KeyCode::Char('y'))).unwrap().answer;
        let PermissionAnswer::AllowComposed { rows } = answer else {
            panic!("expected a composed answer: {answer:?}");
        };
        let lifetimes: Vec<_> = rows
            .iter()
            .map(|row| row.as_ref().map(|row| row.lifetime.clone()))
            .collect();
        assert_eq!(
            lifetimes,
            [
                Some(PermissionLifetime::Project),
                Some(PermissionLifetime::Conversation)
            ]
        );
    }

    #[test_case(None; "focused_row")]
    #[test_case(Some(0); "covered_row_clicked")]
    fn customize_opens_on_the_only_new_rows_ladder(clicked: Option<usize>) {
        let mut request = commands_request(&PAIR);
        cover(
            &mut request,
            0,
            RuleOrigin::Builtin,
            CONFINED_READ_AUTHORITY,
            false,
        );
        let mut prompt = prompt_for(request);
        if let Some(row) = clicked {
            prompt.activate(PromptTarget::Row(row));
        }
        prompt.open_customize(false);
        assert_eq!(prompt.customize.as_ref().unwrap().row, Some(1));
    }

    #[test_case(false; "from_main_view")]
    #[test_case(true; "from_review")]
    fn customize_returns_without_answering(from_review: bool) {
        let mut prompt = if from_review {
            prompt_for(commands_request(&PAIR))
        } else {
            shell_prompt(SINGLE_COMMAND)
        };
        draw(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('e')));
        if from_review {
            prompt.go_to_page(None);
            draw(&mut prompt);
            prompt.handle_key(number_key(2));
        }
        let back = prompt.customize.as_ref().unwrap().back;
        let (rows, answer) = (
            prompt.row_grants(),
            prompt.choice_answer(Choice::Conversation),
        );
        for code in [KeyCode::Down, KeyCode::Right] {
            assert!(prompt.handle_key(key(code)).is_none());
        }
        assert!(prompt.handle_key(key(KeyCode::Esc)).is_none());
        assert!(prompt.customize.is_none());
        assert_eq!(prompt.panel, back);
        assert_eq!(prompt.step.is_some(), from_review);
        assert_eq!(prompt.row_grants(), rows);
        assert_eq!(prompt.choice_answer(Choice::Conversation), answer);
        assert_eq!(prompt.pending_count(), 1);
    }

    #[test]
    fn broad_conversation_grant_needs_a_fresh_second_enter() {
        let mut prompt = broad_pending(PermissionLifetime::Conversation);
        let frozen = prompt.pending.as_ref().unwrap().answer.clone();
        assert!(prompt.pending.as_ref().unwrap().phrases.is_empty());
        assert!(prose(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(PRESS_AGAIN));
        for _ in 0..REPEAT_COUNT {
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        }
        assert!(draw(&mut prompt).contains(REARM_MESSAGE));
        let modified = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(prompt.handle_key(modified).is_none());
        prompt.handle_key(with_kind(KeyCode::Enter, KeyEventKind::Release));
        draw(&mut prompt);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            frozen
        );
    }

    #[test]
    fn broad_project_grant_needs_the_typed_phrase() {
        let mut prompt = broad_pending(PermissionLifetime::Project);
        let pending = prompt.pending.as_ref().unwrap();
        let [phrase] = pending.phrases.as_slice() else {
            panic!("expected one phrase: {:?}", pending.phrases);
        };
        let (phrase, frozen) = (phrase.clone(), pending.answer.clone());
        prompt.handle_key(with_kind(KeyCode::Enter, KeyEventKind::Release));
        draw(&mut prompt);
        assert!(prompt.handle_paste(WRONG_PHRASE));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(prompt.pending.is_some());
        prompt.field.clear();
        assert!(prompt.handle_paste(&phrase));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            frozen
        );
    }

    #[test_case(PermissionLifetime::Conversation, KeyCode::Enter, true; "keypress_enter")]
    #[test_case(PermissionLifetime::Conversation, KeyCode::Char('y'), false; "keypress_letter")]
    #[test_case(PermissionLifetime::Project, KeyCode::Enter, true; "phrase_enter")]
    #[test_case(PermissionLifetime::Project, KeyCode::Char('y'), false; "phrase_letter")]
    fn a_held_key_is_named_only_when_it_would_confirm(
        lifetime: PermissionLifetime,
        held: KeyCode,
        named: bool,
    ) {
        let mut prompt = broad_pending(lifetime);
        let frozen = prompt.pending.as_ref().unwrap().answer.clone();
        prompt.handle_key(with_kind(KeyCode::Enter, KeyEventKind::Release));
        draw(&mut prompt);
        assert!(
            prompt
                .handle_key(with_kind(held, KeyEventKind::Repeat))
                .is_none()
        );
        assert_eq!(draw(&mut prompt).contains(REARM_MESSAGE), named);
        assert!(prompt.handle_key(key(held)).is_none());
        assert_eq!(prompt.pending.as_ref().unwrap().answer, frozen);
    }

    #[test_case(Some('4'), GUIDANCE; "number_with_text")]
    #[test_case(Some('n'), GUIDANCE; "letter_with_text")]
    #[test_case(None, GUIDANCE; "click_with_text")]
    #[test_case(Some('n'), ""; "without_text")]
    fn guidance_choice_denies_with_text(shortcut: Option<char>, guidance: &str) {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        draw(&mut prompt);
        let opened = match shortcut {
            Some(shortcut) => prompt
                .handle_key(key(KeyCode::Char(shortcut)))
                .map(|decision| decision.answer),
            None => decided(click(&mut prompt, PromptTarget::Choice(Choice::Deny))),
        };
        assert!(opened.is_none());
        assert_eq!(prompt.state, PromptState::Guidance);
        assert!(prompt.handle_paste(guidance));
        let expected = if guidance.is_empty() {
            PermissionAnswer::Deny
        } else {
            PermissionAnswer::DenyWithGuidance(guidance.into())
        };
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
            expected
        );
    }

    #[test]
    fn escape_leaves_guidance_without_denying() {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        draw(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('n')));
        assert!(prompt.handle_paste(GUIDANCE));
        assert!(prompt.handle_key(key(KeyCode::Esc)).is_none());
        assert_eq!(prompt.state, PromptState::Normal);
        assert!(prompt.field.is_empty());
    }

    #[test_case(Panel::Main; "main_view")]
    #[test_case(Panel::Details; "details")]
    fn escape_denies_without_guidance(panel: Panel) {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        if panel == Panel::Details {
            prompt.toggle_details();
        }
        draw(&mut prompt);
        assert_eq!(prompt.panel, panel);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Esc)).unwrap().answer,
            PermissionAnswer::Deny
        );
    }

    #[test_case('1'; "once_number")]
    #[test_case('y'; "once")]
    #[test_case('s'; "conversation")]
    #[test_case('a'; "project")]
    fn queued_decisions_need_a_fresh_frame_and_a_fresh_press(shortcut: char) {
        let mut prompt = queued_pair();
        draw(&mut prompt);
        let area = hit(&prompt, &PromptTarget::Choice(Choice::Once));
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        assert!(prompt.resolve(FIRST));
        assert!(prompt.handle_key(key(KeyCode::Char(shortcut))).is_none());
        assert!(draw(&mut prompt).contains(REARM_MESSAGE));
        assert!(
            decided(prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area)))
                .is_none()
        );
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(
                prompt
                    .handle_key(with_kind(KeyCode::Char(shortcut), kind))
                    .is_none()
            );
        }
        assert_eq!(
            prompt
                .handle_key(key(KeyCode::Char(shortcut)))
                .unwrap()
                .request_id,
            NEXT
        );
    }

    #[test_case('y'; "once")]
    #[test_case('s'; "conversation")]
    #[test_case('a'; "project")]
    fn repeat_presses_need_an_explicit_rearming_gesture(shortcut: char) {
        let event = key(KeyCode::Char(shortcut));
        let mut prompt = queued_pair();
        draw(&mut prompt);
        assert_eq!(prompt.handle_key(event).unwrap().request_id, FIRST);
        assert!(prompt.resolve(FIRST));
        assert!(draw(&mut prompt).contains(REARM_MESSAGE));
        for _ in 0..REPEAT_COUNT {
            assert!(prompt.handle_key(event).is_none());
            assert!(draw(&mut prompt).contains(REARM_MESSAGE));
            assert_eq!(prompt.request_id(), Some(NEXT));
        }
        assert!(prompt.handle_key(key(KeyCode::Down)).is_none());
        assert!(!draw(&mut prompt).contains(REARM_MESSAGE));
        assert_eq!(prompt.handle_key(event).unwrap().request_id, NEXT);
    }

    #[test_case("release"; "release")]
    #[test_case("different_key"; "different_key")]
    #[test_case("mouse"; "mouse_press")]
    fn unambiguous_activation_rearms_the_next_request(activation: &str) {
        let mut prompt = queued_pair();
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
        prompt.resolve(FIRST);
        draw(&mut prompt);
        let decided = match activation {
            "release" => {
                prompt.handle_key(with_kind(KeyCode::Char('y'), KeyEventKind::Release));
                prompt.handle_key(key(KeyCode::Char('y'))).is_some()
            }
            "different_key" => prompt.handle_key(key(KeyCode::Char('s'))).is_some(),
            "mouse" => decided(click(&mut prompt, PromptTarget::Choice(Choice::Once))).is_some(),
            _ => unreachable!(),
        };
        assert!(decided);
        assert_eq!(prompt.request_id(), Some(NEXT));
    }

    #[test]
    fn navigation_repeats_and_unrelated_releases_do_not_rearm_an_approval() {
        let mut prompt = queued_pair();
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
        prompt.resolve(FIRST);
        draw(&mut prompt);
        for _ in 0..REPEAT_COUNT {
            prompt.handle_key(with_kind(KeyCode::Down, KeyEventKind::Repeat));
            prompt.handle_key(with_kind(KeyCode::Down, KeyEventKind::Release));
            assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        }
        prompt.handle_key(key(KeyCode::Up));
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
    }

    #[test]
    fn unseen_press_is_not_made_fresh_by_rendering() {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        prompt.handle_key(key(KeyCode::Down));
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_some());
    }

    #[test_case("release"; "release")]
    #[test_case("key"; "different_key")]
    #[test_case("mouse"; "fresh_click")]
    fn mouse_opened_confirmation_blocks_a_previously_held_enter(rearm: &str) {
        let mut prompt = prompt_for(file_request(OUTSIDE_FILE, PermissionResourceAccess::Write));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        draw(&mut prompt);
        assert!(
            decided(click(
                &mut prompt,
                PromptTarget::Choice(Choice::Conversation)
            ))
            .is_none()
        );
        let frozen = prompt.pending.as_ref().unwrap().answer.clone();
        assert!(prompt.pending.as_ref().unwrap().phrases.is_empty());
        draw(&mut prompt);
        for _ in 0..REPEAT_COUNT {
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
            assert_eq!(prompt.pending.as_ref().unwrap().answer, frozen);
        }
        prompt.handle_key(with_kind(KeyCode::Char('x'), KeyEventKind::Release));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        let answer = match rearm {
            "mouse" => decided(click(
                &mut prompt,
                PromptTarget::Choice(Choice::Conversation),
            )),
            "release" | "key" => {
                if rearm == "release" {
                    prompt.handle_key(with_kind(KeyCode::Enter, KeyEventKind::Release));
                } else {
                    prompt.handle_key(key(KeyCode::Down));
                }
                draw(&mut prompt);
                prompt
                    .handle_key(key(KeyCode::Enter))
                    .map(|decision| decision.answer)
            }
            _ => unreachable!(),
        };
        assert_eq!(answer, Some(frozen));
    }

    #[test_case("resize"; "resize")]
    #[test_case("update"; "update")]
    #[test_case("scroll"; "scroll")]
    fn changing_geometry_or_request_cancels_mouse_down(change: &str) {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        render(&mut prompt, 80, 18);
        let area = hit(&prompt, &PromptTarget::Choice(Choice::Once));
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        match change {
            "resize" => {
                render(&mut prompt, 140, 18);
            }
            "update" => {
                prompt.update(Box::new(native_shell_request(SINGLE_COMMAND)));
            }
            "scroll" => prompt.scroll(1),
            _ => unreachable!(),
        }
        render(&mut prompt, 80, 18);
        assert!(
            decided(prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area)))
                .is_none()
        );
    }

    fn mark_default(request: &mut PermissionRequest, row: usize, id: &str) {
        for option in &mut request.options {
            if option.group.as_ref().and_then(|group| group.resource) == Some(row) {
                option.is_default = option.id == id;
            }
        }
    }

    #[test]
    fn row_choices_survive_request_updates() {
        let request = commands_request(&PAIR);
        let mut prompt = prompt_for(request.clone());
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Left)).is_none());
        let chosen = prompt.row_grant(0);
        assert_ne!(chosen, default_grant(&request, 0));

        let exact = format!("{COMMAND_EXACT_PREFIX}1");
        let mut suggested = request.clone();
        mark_default(&mut suggested, 1, &exact);
        assert!(prompt.update(Box::new(suggested.clone())));
        assert_eq!(prompt.row_grant(0), chosen);
        assert_eq!(
            prompt.row_grant(1),
            Some(PermissionRowGrant::Offered(exact))
        );
        assert_eq!(prompt.input_freshness.blocked_key, Some(KeyCode::Left));

        let Some(PermissionRowGrant::Offered(id)) = chosen else {
            panic!("row 0 was not moved to an offered rung: {chosen:?}");
        };
        let mut withdrawn = suggested;
        withdrawn.options.retain(|option| option.id != id);
        assert!(prompt.update(Box::new(withdrawn.clone())));
        assert_eq!(prompt.row_grant(0), default_grant(&withdrawn, 0));
    }

    #[test_case(false; "unchanged")]
    #[test_case(true; "late_caution")]
    fn late_caution_rearms_the_input_barrier(caution: bool) {
        let request = native_shell_request(SINGLE_COMMAND);
        let mut prompt = prompt_for(request.clone());
        draw(&mut prompt);
        prompt.handle_key(key(KeyCode::Down));
        let update = if caution { advised(request) } else { request };
        assert!(prompt.update(Box::new(update)));
        assert_eq!(prompt.input_freshness.blocked_key.is_some(), caution);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        prompt.handle_key(with_kind(KeyCode::Enter, KeyEventKind::Release));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_some());
    }

    #[test]
    fn written_patterns_must_match_the_command() {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        prompt.open_customize(false);
        let own = prompt
            .customize_items()
            .iter()
            .position(|item| *item == ScopeItem::OwnPattern)
            .unwrap();
        prompt.highlight_scope(own);
        draw(&mut prompt);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(prompt.state, PromptState::PatternEditing);
        assert!(prompt.field.is_empty());
        for (pattern, accepted) in [(UNMATCHED_PATTERN, false), (MATCHING_PATTERN, true)] {
            prompt.field.clear();
            assert!(prompt.handle_paste(pattern));
            prompt.handle_key(key(KeyCode::Enter));
            assert_eq!(prompt.state == PromptState::Normal, accepted, "{pattern}");
        }
        draw(&mut prompt);
        let answer = prompt.handle_key(key(KeyCode::Enter)).unwrap().answer;
        let PermissionAnswer::AllowComposed { rows } = answer else {
            panic!("expected a composed answer: {answer:?}");
        };
        assert_eq!(
            rows[0].as_ref().unwrap().grant,
            PermissionRowGrant::Written(MATCHING_PATTERN.into())
        );
    }

    #[test_case(false; "plain")]
    #[test_case(true; "coloured")]
    fn credentials_never_reach_the_screen(colours: bool) {
        if colours {
            coloured();
        }
        let mut prompt = shell_prompt(SENSITIVE_COMMAND);
        assert!(!draw(&mut prompt).contains(SECRET));
        prompt.toggle_details();
        assert!(!draw(&mut prompt).contains(SECRET));
        prompt.toggle_details();
        prompt.open_customize(false);
        assert!(!draw(&mut prompt).contains(SECRET));
    }

    #[test]
    fn repeats_never_open_guidance_or_send_it() {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        draw(&mut prompt);
        prompt.handle_key(with_kind(KeyCode::Char('n'), KeyEventKind::Repeat));
        assert_eq!(prompt.state, PromptState::Normal);
        prompt.handle_key(with_kind(KeyCode::Char('n'), KeyEventKind::Release));
        prompt.handle_key(key(KeyCode::Char('n')));
        assert!(prompt.handle_paste("abc"));
        prompt.handle_key(with_kind(KeyCode::Backspace, KeyEventKind::Repeat));
        assert_eq!(prompt.field.text(), "ab");
        assert!(
            prompt
                .handle_key(with_kind(KeyCode::Enter, KeyEventKind::Repeat))
                .is_none()
        );
        assert_eq!(prompt.state, PromptState::Guidance);
    }

    #[test]
    fn a_pending_phrase_takes_letters_that_are_shortcuts_elsewhere() {
        let mut prompt = broad_pending(PermissionLifetime::Project);
        prompt.handle_key(with_kind(KeyCode::Enter, KeyEventKind::Release));
        draw(&mut prompt);
        for letter in SHORTCUT_LETTERS.chars() {
            assert!(prompt.handle_key(key(KeyCode::Char(letter))).is_none());
        }
        assert_eq!(prompt.field.text(), SHORTCUT_LETTERS);
        assert_eq!(prompt.panel, Panel::Customize);
        assert!(prompt.pending.is_some());
    }

    #[test]
    fn ctrl_w_edits_the_guidance_instead_of_being_dropped() {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        draw(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('n')));
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
        assert_eq!(prompt.state, PromptState::Guidance);
    }

    #[test_case(true; "selection_copies")]
    #[test_case(false; "no_selection_denies")]
    fn ctrl_c_copies_a_selected_field_before_denying(selected: bool) {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        draw(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_paste(GUIDANCE);
        if selected {
            prompt.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        }
        let decision = prompt.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        if selected {
            assert!(decision.is_none());
            assert_eq!(prompt.take_copied().as_deref(), Some(GUIDANCE));
            assert_eq!(prompt.state, PromptState::Guidance);
        } else {
            assert_eq!(decision.unwrap().answer, PermissionAnswer::Deny);
            assert!(prompt.take_copied().is_none());
        }
    }
}
