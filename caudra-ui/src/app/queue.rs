//! Queue for messages typed while the agent is busy.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use caudra_agent::AgentInput;
use caudra_agent::{PromptAdmission, QueueDelivery, QueueItemId};
use caudra_providers::{ImageMediaType, ImageSource};

use super::{Action, App, PendingRun, Status, format_with_images};

use crate::agent::shared_queue::{QueueItem, QueueSender};
use crate::components::input::{InputAction, InputState, Submission};
use crate::components::queue_panel::{QueueEntry, set_movement_flags};
use crate::input_document::InputDraft;
use crate::theme;

pub(crate) use crate::agent::shared_queue::QueuedMessage;

pub(crate) const EMPTY_PROMPT_ERR: &str = "prompt is empty";
pub(crate) const NO_QUEUE_ERR: &str = "session cannot queue messages";
pub(crate) const REPLACE_BUSY_ERR: &str = "session is already stopping a run";

pub(crate) enum SubmitOutcome {
    Started(Vec<Action>),
    Queued,
    Replacing(Vec<Action>),
    Rejected(&'static str),
}

pub(super) struct QueueEditor {
    target: QueueTarget,
    id: QueueItemId,
    previous_input: InputState,
}

enum QueueTarget {
    Main,
    Task(String),
}

#[derive(Default)]
pub(crate) struct MessageQueue {
    shared: Option<QueueSender>,
    selected: Option<QueueItemId>,
    viewport: usize,
}

impl MessageQueue {
    pub(crate) fn set_shared(&mut self, shared: QueueSender) {
        self.shared = Some(shared);
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.shared.is_some()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.shared.as_ref().is_none_or(|s| s.is_empty())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.len())
    }

    pub(crate) fn remove_id(&mut self, id: QueueItemId) -> bool {
        let removed = self
            .shared
            .as_ref()
            .is_some_and(|shared| shared.remove_id(id).is_some());
        if removed {
            self.clamp_focus();
        }
        removed
    }

    pub(crate) fn clear(&mut self) {
        if let Some(ref shared) = self.shared {
            shared.clear();
        }
        self.selected = None;
        self.viewport = 0;
    }

    pub(crate) fn resume(&self) {
        if let Some(shared) = &self.shared {
            shared.resume();
        }
    }

    pub(crate) fn disconnect(&mut self) {
        self.clear();
        self.shared = None;
    }

    pub(crate) fn focus(&self) -> Option<usize> {
        let selected = self.selected?;
        self.panel_entries()
            .iter()
            .position(|entry| entry.id == selected)
    }

    pub(crate) fn unfocus(&mut self) {
        self.selected = None;
    }

    pub(crate) fn panel_len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.panel_len())
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        self.shared.as_ref().map_or(vec![], |s| s.panel_entries())
    }

    #[cfg(test)]
    pub(crate) fn text_messages(&self) -> Vec<String> {
        self.shared.as_ref().map_or(vec![], |s| s.text_messages())
    }

    pub(crate) fn pending_prompts(&self) -> Vec<crate::agent::shared_queue::PendingPrompt> {
        self.shared
            .as_ref()
            .map_or(vec![], QueueSender::pending_prompts)
    }

    pub(crate) fn begin_edit(&self, id: QueueItemId) -> Option<InputState> {
        self.shared.as_ref()?.begin_edit(id)
    }

    pub(crate) fn finish_edit(&self, id: QueueItemId, message: QueuedMessage) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|shared| shared.finish_edit(id, message))
    }

    pub(crate) fn cancel_edit(&self, id: QueueItemId) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|shared| shared.cancel_edit(id))
    }

    pub(crate) fn set_admission(&self, id: QueueItemId, admission: PromptAdmission) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|shared| shared.set_admission(id, admission))
    }

    pub(crate) fn move_item(&mut self, id: QueueItemId, up: bool) -> bool {
        let moved = self.shared.as_ref().is_some_and(|shared| {
            if up {
                shared.move_up(id)
            } else {
                shared.move_down(id)
            }
        });
        if moved {
            self.select(id);
        }
        moved
    }

    pub(crate) fn delivery(&self) -> QueueDelivery {
        self.shared
            .as_ref()
            .map_or(QueueDelivery::Separate, QueueSender::delivery)
    }

    pub(crate) fn set_delivery(&self, delivery: QueueDelivery) {
        if let Some(shared) = &self.shared {
            shared.set_delivery(delivery);
        }
    }

    pub(crate) fn toggle_delivery(&self) -> QueueDelivery {
        self.shared
            .as_ref()
            .map_or(QueueDelivery::Separate, |shared| shared.toggle_delivery())
    }

    pub(crate) fn viewport(&self) -> usize {
        self.viewport
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        let max = self
            .panel_len()
            .saturating_sub(crate::components::queue_panel::max_visible_rows());
        self.viewport = self
            .viewport
            .saturating_add_signed(-delta as isize)
            .min(max);
        let visible = crate::components::queue_panel::max_visible_rows();
        if let Some(index) = self.focus()
            && (index < self.viewport || index >= self.viewport + visible)
        {
            let next = if index < self.viewport {
                self.viewport
            } else {
                self.viewport + visible - 1
            };
            self.set_focus_at(next);
        }
    }

    fn clamp_focus(&mut self) {
        let entries = self.panel_entries();
        if self
            .selected
            .is_some_and(|id| entries.iter().all(|entry| entry.id != id))
        {
            self.selected = entries.first().map(|entry| entry.id);
        }
        let max = entries
            .len()
            .saturating_sub(crate::components::queue_panel::max_visible_rows());
        self.viewport = self.viewport.min(max);
    }

    pub(crate) fn set_focus_at(&mut self, index: usize) {
        if let Some(id) = self.panel_entries().get(index).map(|entry| entry.id) {
            self.selected = Some(id);
            let visible = crate::components::queue_panel::max_visible_rows();
            if index < self.viewport {
                self.viewport = index;
            } else if index >= self.viewport + visible {
                self.viewport = index + 1 - visible;
            }
        }
    }

    pub(crate) fn select(&mut self, id: QueueItemId) {
        if let Some(index) = self.panel_entries().iter().position(|entry| entry.id == id) {
            self.set_focus_at(index);
        }
    }
}

impl App {
    pub(super) fn active_queue_entries(&self) -> Vec<QueueEntry<'static>> {
        if self.is_main_chat() {
            return self.queue.panel_entries();
        }
        let Some(task_id) = self.active_subagent_id() else {
            return Vec::new();
        };
        let mut pending = self
            .pending_subagent_steers
            .get(task_id)
            .into_iter()
            .flatten()
            .map(|item| QueueEntry {
                id: item.id,
                text: Cow::Owned(item.text.clone()),
                color: theme::current().foreground,
                editable: true,
                movable: false,
                can_move_up: false,
                can_move_down: false,
                admission: Some(PromptAdmission::Steer),
            })
            .collect::<Vec<_>>();
        if self.subagent_steers.contains_key(task_id) {
            set_movement_flags(&mut pending);
        }
        let mut unsent = self
            .unsent_subagent_steers
            .get(task_id)
            .into_iter()
            .flatten()
            .map(|item| QueueEntry {
                id: item.id,
                text: Cow::Owned(item.text.clone()),
                color: theme::current().foreground,
                editable: true,
                movable: true,
                can_move_up: false,
                can_move_down: false,
                admission: Some(PromptAdmission::Steer),
            })
            .collect::<Vec<_>>();
        set_movement_flags(&mut unsent);
        pending.extend(unsent);
        pending
    }

    pub(super) fn active_queue_title(&self) -> String {
        let kind = if !self.is_main_chat()
            && self.active_subagent_id().is_some_and(|id| {
                !self.pending_subagent_steers.contains_key(id)
                    && self.unsent_subagent_steers.contains_key(id)
            }) {
            "Unsent"
        } else {
            "Queue"
        };
        format!("{kind} - {}", self.chats[self.active_chat].name)
    }

    pub(super) fn active_queue_delivery(&self) -> Option<QueueDelivery> {
        if self.queue_editor_active() {
            return None;
        }
        let entries = self.active_queue_entries();
        if self.is_main_chat() {
            return entries
                .iter()
                .any(|entry| entry.admission == Some(PromptAdmission::Queue))
                .then(|| self.queue.delivery());
        }
        if !entries.iter().any(|entry| entry.editable) {
            return None;
        }
        self.active_subagent_id()
            .and_then(|task_id| self.subagent_steers.get(task_id))
            .map(caudra_agent::SteeringQueue::delivery)
    }

    pub(super) fn toggle_active_queue_delivery(&mut self) {
        if self.active_queue_delivery().is_none() {
            return;
        }
        let delivery = if self.is_main_chat() {
            self.queue.toggle_delivery()
        } else {
            let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
                return;
            };
            let Some(queue) = self.subagent_steers.get(&task_id) else {
                return;
            };
            queue.toggle_delivery()
        };
        let entries = self.active_queue_entries();
        let count = entries
            .iter()
            .filter(|entry| {
                entry.editable
                    && !entry.movable
                    && (!self.is_main_chat() || entry.admission == Some(PromptAdmission::Queue))
            })
            .count();
        self.flash(match delivery {
            QueueDelivery::Separate => "Queued messages will be sent separately".into(),
            QueueDelivery::TogetherNextTurn => {
                format!("{count} queued message(s) will be sent together next turn")
            }
        });
    }

    pub(super) fn active_queue_focus(&self) -> Option<usize> {
        if self.is_main_chat() {
            return self.queue.focus();
        }
        let (task_id, selected) = self.task_queue_selection.as_ref()?;
        if self.active_subagent_id()? != task_id {
            return None;
        }
        self.active_queue_entries()
            .iter()
            .position(|entry| entry.id == *selected)
    }

    pub(super) fn active_queue_viewport(&self) -> usize {
        if self.is_main_chat() {
            self.queue.viewport()
        } else {
            self.task_queue_viewport
        }
    }

    pub(super) fn select_active_queue_item(&mut self, id: QueueItemId) {
        if self.is_main_chat() {
            self.queue.select(id);
            return;
        }
        let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
            return;
        };
        if let Some(index) = self
            .active_queue_entries()
            .iter()
            .position(|entry| entry.id == id)
        {
            self.task_queue_selection = Some((task_id, id));
            self.ensure_task_queue_visible(index);
        }
    }

    pub(super) fn focus_active_queue(&mut self) {
        if let Some(id) = self.active_queue_entries().first().map(|entry| entry.id) {
            self.select_active_queue_item(id);
        }
    }

    pub(super) fn unfocus_active_queue(&mut self) {
        if self.is_main_chat() {
            self.queue.unfocus();
        } else {
            self.task_queue_selection = None;
        }
    }

    pub(super) fn active_queue_is_focused(&self) -> bool {
        self.active_queue_focus().is_some()
    }

    pub(super) fn move_active_queue_focus(&mut self, delta: isize) {
        let Some(index) = self.active_queue_focus() else {
            return;
        };
        let entries = self.active_queue_entries();
        let next = index
            .saturating_add_signed(delta)
            .min(entries.len().saturating_sub(1));
        if let Some(entry) = entries.get(next) {
            self.select_active_queue_item(entry.id);
        }
    }

    pub(super) fn move_active_queue_item(&mut self, id: QueueItemId, up: bool) -> bool {
        let allowed = self.active_queue_entries().iter().any(|entry| {
            entry.id == id
                && if up {
                    entry.can_move_up
                } else {
                    entry.can_move_down
                }
        });
        if !allowed {
            return false;
        }
        let moved = if self.is_main_chat() {
            self.queue.move_item(id, up)
        } else {
            let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
                return false;
            };
            if self
                .pending_subagent_steers
                .get(&task_id)
                .is_some_and(|items| items.iter().any(|item| item.id == id))
            {
                let queue = self.subagent_steers.get(&task_id);
                let queue_moved = queue.is_some_and(|queue| {
                    if up {
                        queue.move_up(id)
                    } else {
                        queue.move_down(id)
                    }
                });
                let mirror_moved = queue_moved
                    && self
                        .pending_subagent_steers
                        .get_mut(&task_id)
                        .is_some_and(|items| swap_pending(items, id, up));
                if queue_moved
                    && !mirror_moved
                    && let Some(queue) = queue
                {
                    if up {
                        queue.move_down(id);
                    } else {
                        queue.move_up(id);
                    }
                }
                mirror_moved
            } else {
                self.unsent_subagent_steers
                    .get_mut(&task_id)
                    .is_some_and(|items| swap_pending(items, id, up))
            }
        };
        if moved {
            self.select_active_queue_item(id);
        } else {
            self.clamp_active_queue_focus();
        }
        moved
    }

    pub(super) fn move_focused_queue_item(&mut self, up: bool) {
        if let Some(id) = self
            .active_queue_entries()
            .get(self.active_queue_focus().unwrap_or(0))
            .map(|entry| entry.id)
        {
            self.move_active_queue_item(id, up);
        }
    }

    pub(super) fn scroll_active_queue(&mut self, delta: i32) {
        if self.is_main_chat() {
            self.queue.scroll(delta);
            return;
        }
        let max = self
            .active_queue_entries()
            .len()
            .saturating_sub(crate::components::queue_panel::max_visible_rows());
        self.task_queue_viewport = self
            .task_queue_viewport
            .saturating_add_signed(-delta as isize)
            .min(max);
        let visible = crate::components::queue_panel::max_visible_rows();
        if let Some(index) = self.active_queue_focus()
            && (index < self.task_queue_viewport || index >= self.task_queue_viewport + visible)
        {
            let next = if index < self.task_queue_viewport {
                self.task_queue_viewport
            } else {
                self.task_queue_viewport + visible - 1
            };
            if let Some(id) = self.active_queue_entries().get(next).map(|entry| entry.id) {
                self.select_active_queue_item(id);
            }
        }
    }

    pub(super) fn delete_active_queue_item(&mut self, id: QueueItemId) -> bool {
        let removed = if self.is_main_chat() {
            self.queue.remove_id(id)
        } else {
            let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
                return false;
            };
            let is_pending = self
                .pending_subagent_steers
                .get(&task_id)
                .is_some_and(|items| items.iter().any(|item| item.id == id));
            if is_pending {
                if self
                    .subagent_steers
                    .get(&task_id)
                    .and_then(|queue| queue.remove(id))
                    .is_none()
                {
                    return false;
                }
                remove_pending(&mut self.pending_subagent_steers, &task_id, id)
            } else {
                remove_pending(&mut self.unsent_subagent_steers, &task_id, id)
            }
        };
        if removed {
            if self.replacement_item == Some(id) {
                self.replacement_item = None;
                self.status = if self.cancelling_run.is_some() || self.queue.panel_len() > 0 {
                    Status::Streaming
                } else {
                    Status::Idle
                };
            }
            self.clamp_active_queue_focus();
        }
        removed
    }

    pub(super) fn delete_focused_queue_item(&mut self) {
        if let Some(id) = self
            .active_queue_entries()
            .get(self.active_queue_focus().unwrap_or(0))
        {
            self.delete_active_queue_item(id.id);
        }
    }

    pub(super) fn set_focused_queue_admission(&mut self, admission: PromptAdmission) {
        if !self.is_main_chat() {
            return;
        }
        let Some(id) = self
            .active_queue_entries()
            .get(self.active_queue_focus().unwrap_or(0))
            .filter(|entry| {
                matches!(
                    entry.admission,
                    Some(PromptAdmission::Queue | PromptAdmission::Steer)
                )
            })
            .map(|entry| entry.id)
        else {
            return;
        };
        if self.queue.set_admission(id, admission) {
            self.queue.select(id);
            self.flash(match admission {
                PromptAdmission::Queue => "Prompt moved to Up next".into(),
                PromptAdmission::Steer => "Prompt will guide the current run".into(),
                PromptAdmission::Interrupt => return,
            });
        }
    }

    pub(super) fn pop_active_queue(&mut self) {
        if let Some(id) = self.active_queue_entries().first().map(|entry| entry.id) {
            self.delete_active_queue_item(id);
        }
    }

    pub(super) fn begin_queue_edit(&mut self, id: QueueItemId) {
        if self.queue_editor.is_some() {
            return;
        }
        let (target, input) = if self.is_main_chat() {
            let Some(input) = self.queue.begin_edit(id) else {
                self.flash("Queued message was already sent".into());
                self.clamp_active_queue_focus();
                return;
            };
            (QueueTarget::Main, input)
        } else {
            let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
                return;
            };
            let pending = self
                .pending_subagent_steers
                .get(&task_id)
                .and_then(|items| items.iter().find(|item| item.id == id));
            let Some(item) = pending.or_else(|| {
                self.unsent_subagent_steers
                    .get(&task_id)
                    .and_then(|items| items.iter().find(|item| item.id == id))
            }) else {
                self.clamp_active_queue_focus();
                return;
            };
            let draft = item.draft.clone();
            if pending.is_some()
                && let Some(queue) = self.subagent_steers.get(&task_id)
                && queue.begin_edit(id).is_none()
            {
                self.flash("Queued message was already sent".into());
                self.clamp_active_queue_focus();
                return;
            }
            (
                QueueTarget::Task(task_id),
                InputState::new(draft, Vec::new()),
            )
        };
        let previous_input = self.active_input_box_mut().take_state();
        self.active_input_box_mut().set_state(input);
        self.queue_editor = Some(QueueEditor {
            target,
            id,
            previous_input,
        });
    }

    pub(super) fn handle_queue_editor_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Vec<Action> {
        match self.active_input_box_mut().handle_editor_key(key) {
            InputAction::Submit(sub) => self.save_queue_edit(sub),
            InputAction::EditPaste(id) => {
                self.open_paste_editor(id);
                Vec::new()
            }
            InputAction::Passthrough(key) if key.code == crossterm::event::KeyCode::Esc => {
                self.cancel_queue_edit();
                Vec::new()
            }
            InputAction::Passthrough(_)
            | InputAction::ContinueLine
            | InputAction::PaletteSync(_)
            | InputAction::None => Vec::new(),
        }
    }

    fn save_queue_edit(&mut self, sub: Submission) -> Vec<Action> {
        if sub.text.trim().is_empty() {
            self.active_input_box_mut().set_draft(sub.draft);
            self.flash("Queued message cannot be empty".into());
            return Vec::new();
        }
        let Some(editor) = self.queue_editor.take() else {
            return Vec::new();
        };
        let saved = match &editor.target {
            QueueTarget::Main => self.queue.finish_edit(editor.id, sub.into()),
            QueueTarget::Task(task_id) => {
                let queue_saved = self
                    .subagent_steers
                    .get(task_id)
                    .is_some_and(|queue| queue.finish_edit(editor.id, sub.text.clone()));
                let pending_saved = update_pending(
                    if queue_saved {
                        &mut self.pending_subagent_steers
                    } else {
                        &mut self.unsent_subagent_steers
                    },
                    task_id,
                    editor.id,
                    sub.text,
                    sub.draft,
                );
                queue_saved || pending_saved
            }
        };
        self.active_input_box_mut().set_state(editor.previous_input);
        if !saved {
            self.flash("Queued message was already sent".into());
        }
        Vec::new()
    }

    pub(super) fn cancel_queue_edit(&mut self) {
        let Some(editor) = self.queue_editor.take() else {
            return;
        };
        match &editor.target {
            QueueTarget::Main => {
                self.queue.cancel_edit(editor.id);
            }
            QueueTarget::Task(task_id) => {
                if let Some(queue) = self.subagent_steers.get(task_id) {
                    queue.cancel_edit(editor.id);
                }
            }
        }
        self.active_input_box_mut().set_state(editor.previous_input);
    }

    pub(super) fn move_unsent_to_main(&mut self, id: QueueItemId) {
        let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
            return;
        };
        let Some((index, item)) = self
            .unsent_subagent_steers
            .get_mut(&task_id)
            .and_then(|items| {
                let index = items.iter().position(|item| item.id == id)?;
                Some((index, items.remove(index).expect("known unsent index")))
            })
        else {
            return;
        };
        if !self.queue_with_admission(
            QueuedMessage {
                text: item.text.clone(),
                images: Vec::new(),
                paste_ranges: Vec::new(),
            },
            PromptAdmission::Queue,
        ) {
            self.unsent_subagent_steers
                .entry(task_id)
                .or_default()
                .insert(index, item);
            self.flash(NO_QUEUE_ERR.into());
            return;
        }
        if self
            .unsent_subagent_steers
            .get(&task_id)
            .is_some_and(std::collections::VecDeque::is_empty)
        {
            self.unsent_subagent_steers.remove(&task_id);
        }
        self.clamp_active_queue_focus();
    }

    pub(super) fn queue_editor_active(&self) -> bool {
        self.queue_editor.is_some()
    }

    fn ensure_task_queue_visible(&mut self, index: usize) {
        let visible = crate::components::queue_panel::max_visible_rows();
        if index < self.task_queue_viewport {
            self.task_queue_viewport = index;
        } else if index >= self.task_queue_viewport + visible {
            self.task_queue_viewport = index + 1 - visible;
        }
    }

    pub(super) fn clamp_active_queue_focus(&mut self) {
        if self.is_main_chat() {
            self.queue.clamp_focus();
            return;
        }
        let entries = self.active_queue_entries();
        if let Some((task_id, selected)) = &self.task_queue_selection
            && (self.active_subagent_id() != Some(task_id)
                || entries.iter().all(|entry| entry.id != *selected))
        {
            self.task_queue_selection = entries.first().and_then(|entry| {
                self.active_subagent_id()
                    .map(|task_id| (task_id.to_owned(), entry.id))
            });
        }
        let max = entries
            .len()
            .saturating_sub(crate::components::queue_panel::max_visible_rows());
        self.task_queue_viewport = self.task_queue_viewport.min(max);
    }

    /// The one queue-or-start decision, shared by the keyboard and Lua
    /// paths so they cannot drift. Expects raw text: interpretation (slash
    /// commands, `exit`, `!`) is the caller's job, or skipped on purpose.
    #[cfg(test)]
    pub(crate) fn submit_prompt(&mut self, msg: QueuedMessage) -> SubmitOutcome {
        self.submit_prompt_with_admission(msg, PromptAdmission::Queue)
    }

    pub(crate) fn submit_prompt_with_admission(
        &mut self,
        msg: QueuedMessage,
        admission: PromptAdmission,
    ) -> SubmitOutcome {
        if msg.text.trim().is_empty() && msg.images.is_empty() {
            return SubmitOutcome::Rejected(EMPTY_PROMPT_ERR);
        }
        if self
            .state
            .session
            .meta
            .pending_revert
            .as_ref()
            .is_some_and(|pending| pending.restore_operation.is_some())
        {
            return SubmitOutcome::Rejected(super::REVERT_BUSY_MSG);
        }
        if self.status == Status::Streaming {
            if admission == PromptAdmission::Interrupt {
                return self.replace_and_notify(msg);
            }
            if self.queue_with_admission(msg, admission) {
                SubmitOutcome::Queued
            } else {
                SubmitOutcome::Rejected(NO_QUEUE_ERR)
            }
        } else {
            SubmitOutcome::Started(self.start_from_queue(&msg))
        }
    }

    /// Keyboard path: nobody is around to receive an `Err`, so
    /// rejections flash on screen instead.
    pub(super) fn submit_or_queue(&mut self, msg: QueuedMessage) -> Vec<Action> {
        self.submit_or_queue_with_admission(msg, PromptAdmission::Queue)
    }

    pub(super) fn submit_or_queue_with_admission(
        &mut self,
        msg: QueuedMessage,
        admission: PromptAdmission,
    ) -> Vec<Action> {
        match self.submit_prompt_with_admission(msg, admission) {
            SubmitOutcome::Started(actions) => actions,
            SubmitOutcome::Queued => vec![],
            SubmitOutcome::Replacing(actions) => actions,
            SubmitOutcome::Rejected(e) => {
                self.flash(e.into());
                vec![]
            }
        }
    }

    pub(super) fn submit_goal(&mut self, condition: &str) -> Vec<Action> {
        let msg = QueuedMessage {
            text: condition.to_owned(),
            images: Vec::new(),
            paste_ranges: Vec::new(),
        };
        let mut input = self.build_agent_input(&msg);
        input.preamble.push(caudra_providers::Message::synthetic(
            caudra_agent::goal_kickoff_message(condition),
        ));
        if self.status == Status::Streaming {
            let Some(ref shared) = self.queue.shared else {
                self.flash(NO_QUEUE_ERR.into());
                return vec![];
            };
            shared.push(QueueItem::Message {
                text: msg.text,
                image_count: 0,
                paste_ranges: msg.paste_ranges,
                input,
                run_id: self.run_id,
                admission: PromptAdmission::Queue,
                displayed: false,
            });
            vec![]
        } else {
            self.start_run(input, msg.text)
        }
    }

    /// Deferred path: the agent is busy, so park the message and let
    /// `QueueItemConsumed` draw it once the agent picks it up. Returns
    /// false when there is no shared queue, meaning the message was dropped.
    #[cfg(test)]
    pub(super) fn queue_and_notify(&mut self, msg: QueuedMessage) -> bool {
        self.queue_with_admission(msg, PromptAdmission::Queue)
    }

    pub(super) fn queue_with_admission(
        &mut self,
        msg: QueuedMessage,
        admission: PromptAdmission,
    ) -> bool {
        let Some(ref shared) = self.queue.shared else {
            return false;
        };
        let input = self.build_agent_input(&msg);
        shared.push(QueueItem::Message {
            text: msg.text,
            image_count: msg.images.len(),
            paste_ranges: msg.paste_ranges,
            input,
            run_id: self.run_id,
            admission,
            displayed: false,
        });
        true
    }

    fn replace_and_notify(&mut self, msg: QueuedMessage) -> SubmitOutcome {
        let Some(shared) = self.queue.shared.clone() else {
            return SubmitOutcome::Rejected(NO_QUEUE_ERR);
        };
        let input = self.build_agent_input(&msg);
        let mut replacement = QueueItem::Message {
            text: msg.text,
            image_count: msg.images.len(),
            paste_ranges: msg.paste_ranges,
            input,
            run_id: self.run_id,
            admission: PromptAdmission::Interrupt,
            displayed: false,
        };
        if self.replacement_item.is_some() {
            match shared.update_pending_replacement(self.run_id, replacement) {
                Ok(id) => {
                    self.replacement_item = Some(id);
                    return SubmitOutcome::Replacing(Vec::new());
                }
                Err(item) => {
                    replacement = *item;
                    self.replacement_item = None;
                    self.cancelling_run = None;
                }
            }
        }
        if self.cancelling_run.is_some() {
            return SubmitOutcome::Rejected(REPLACE_BUSY_ERR);
        }
        let cancelled_run = self.run_id;
        let replacement_run = cancelled_run + 1;
        let (id, active) = shared.replace(cancelled_run, replacement_run, replacement);
        let cancelled_run = self.begin_main_cancel(true, active);
        debug_assert_eq!(self.run_id, replacement_run);
        self.replacement_item = Some(id);
        SubmitOutcome::Replacing(
            active
                .then_some(Action::CancelAgent {
                    run_id: cancelled_run,
                })
                .into_iter()
                .collect(),
        )
    }

    /// Push restored queue items only here, never in `restore_display`: on
    /// load/rewind the display is restored before `respawn` swaps the shared
    /// queue, so pushing earlier would fill a queue that is about to die.
    pub(crate) fn flush_restored_queue(&mut self) {
        // The live queue owns the prompts from here on, so the recovery
        // snapshot must stop overriding it on save.
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        if self.state.session.meta.queued_messages_together {
            self.queue.set_delivery(QueueDelivery::TogetherNextTurn);
        }
        if !self.state.session.meta.queued_messages.is_empty() {
            self.status = Status::Streaming;
        }
        // Read, not taken: the live queue is what the next checkpoint mirrors
        // back into the session, so emptying it here changes nothing on disk.
        for (index, prompt) in self
            .state
            .session
            .meta
            .queued_messages
            .clone()
            .into_iter()
            .enumerate()
        {
            let admission = match self
                .state
                .session
                .meta
                .queued_message_admissions
                .get(index)
                .copied()
                .unwrap_or_default()
            {
                caudra_storage::sessions::StoredPromptAdmission::Queue => PromptAdmission::Queue,
                caudra_storage::sessions::StoredPromptAdmission::Steer => PromptAdmission::Steer,
                caudra_storage::sessions::StoredPromptAdmission::Interrupt => {
                    PromptAdmission::Interrupt
                }
            };
            let images = prompt
                .images
                .into_iter()
                .filter_map(|image| {
                    let Some(media_type) = ImageMediaType::from_mime(&image.media_type) else {
                        tracing::warn!(
                            media_type = %image.media_type,
                            "skipping stored queued image"
                        );
                        return None;
                    };
                    Some(ImageSource::new(media_type, image.data.into()))
                })
                .collect();
            self.queue_with_admission(
                QueuedMessage {
                    text: prompt.text,
                    images,
                    paste_ranges: prompt
                        .paste_ranges
                        .into_iter()
                        .map(|range| range.start..range.end)
                        .collect(),
                },
                admission,
            );
        }
    }

    pub(super) fn queue_compact(&mut self) {
        let Some(ref shared) = self.queue.shared else {
            return;
        };
        shared.push(QueueItem::Compact {
            run_id: self.run_id,
        });
    }

    /// Agent reached a deferred message: time to draw the bubble. Restored
    /// queue items start runs without `start_run`, so this is where the app
    /// learns the agent is busy. Immediate-dispatch items skip this event,
    /// so no dedup needed.
    pub(super) fn on_queue_item_consumed(
        &mut self,
        id: QueueItemId,
        text: &str,
        image_count: usize,
    ) {
        if self.replacement_item == Some(id) {
            self.replacement_item = None;
            self.cancelling_run = None;
        }
        self.queue.clamp_focus();
        self.status = Status::Streaming;
        self.main_chat()
            .show_user_message(format_with_images(text, image_count));
    }

    pub(super) fn on_queue_batch_consumed(&mut self, items: &[caudra_agent::QueueConsumedItem]) {
        if self
            .replacement_item
            .is_some_and(|id| items.iter().any(|item| item.id == id))
        {
            self.replacement_item = None;
            self.cancelling_run = None;
        }
        self.queue.clamp_focus();
        self.status = Status::Streaming;
        let messages = items
            .iter()
            .map(|item| format_with_images(&item.text, item.image_count));
        self.main_chat().show_user_messages(messages);
    }

    /// Immediate path: kick off the agent and draw the bubble in the same
    /// frame, so the user sees their message land where it will stay.
    pub(super) fn start_from_queue(&mut self, msg: &QueuedMessage) -> Vec<Action> {
        let display = format_with_images(&msg.text, msg.images.len());
        let input = self.build_agent_input(msg);
        self.start_run(input, display)
    }

    pub(crate) fn start_mailbox_run(
        &mut self,
        preamble: Vec<caudra_providers::Message>,
    ) -> Vec<Action> {
        let mut input = self.build_agent_input(&QueuedMessage {
            text: String::new(),
            images: Vec::new(),
            paste_ranges: Vec::new(),
        });
        input.preamble = preamble;
        self.start_run(input, String::new())
    }

    pub(crate) fn start_goal_checkin(&mut self) -> Vec<Action> {
        let Some(goal) = self.state.goal.snapshot() else {
            self.goal_deferred = false;
            return vec![];
        };
        let mut input = self.build_agent_input(&QueuedMessage {
            text: String::new(),
            images: Vec::new(),
            paste_ranges: Vec::new(),
        });
        input.preamble.push(caudra_providers::Message::synthetic(
            caudra_agent::goal_checkin_message(&goal.condition),
        ));
        self.start_run(input, String::new())
    }

    /// The one place a fresh run starts: every path that emits
    /// `Action::SendMessage` must go through here so `run_id` bumps exactly
    /// once per run.
    pub(super) fn start_run(&mut self, input: AgentInput, display: String) -> Vec<Action> {
        if self.cancelling_run.is_some()
            || self
                .state
                .session
                .meta
                .pending_revert
                .as_ref()
                .is_some_and(|pending| pending.restore_operation.is_some())
        {
            self.flash(super::REVERT_BUSY_MSG.into());
            return Vec::new();
        }
        self.run_id += 1;
        self.goal_deferred = false;
        self.clear_exit_request();
        // New work supersedes text held for recovery after an agent error.
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        // Streaming from here, before the snapshot lands, so the spinner runs
        // and a second submit queues instead of racing a second run.
        self.status = Status::Streaming;
        let run_id = self.run_id;
        self.pending_run = Some(PendingRun {
            run_id,
            input,
            display,
        });
        vec![Action::SnapshotWorkspace {
            run_id,
            store: Arc::clone(&self.snapshot_store),
            cwd: PathBuf::from(&self.state.session.cwd),
            head: self.history_head(),
        }]
    }

    /// Second half of [`Self::start_run`], resumed once the capture is done.
    /// A failure leaves the run unstarted, matching the inline behaviour it
    /// replaces: the user sees a flash and no message bubble.
    pub(crate) fn on_workspace_snapshot(
        &mut self,
        run_id: u64,
        result: Result<(), String>,
    ) -> Vec<Action> {
        let Some(pending) = self.pending_run.take_if(|run| run.run_id == run_id) else {
            return Vec::new();
        };
        if let Err(error) = result {
            self.flash(format!("Failed to snapshot workspace: {error}"));
            self.status = Status::Idle;
            // The bubble was never drawn and the queue never saw this text, so
            // dropping `pending` here is the only thing standing between the
            // user and losing what they typed.
            if !pending.display.is_empty() {
                self.input_box.buffer.insert_text(&pending.display);
            }
            return Vec::new();
        }
        self.fire_session_autocmd("TurnStart", serde_json::json!({}));
        if !pending.display.is_empty() {
            self.main_chat().show_user_message(pending.display);
        }
        vec![Action::SendMessage(Box::new(pending.input))]
    }
}

fn swap_pending(
    items: &mut std::collections::VecDeque<super::PendingSteer>,
    id: QueueItemId,
    up: bool,
) -> bool {
    let Some(index) = items.iter().position(|item| item.id == id) else {
        return false;
    };
    let neighbor = if up {
        index.checked_sub(1)
    } else {
        (index + 1 < items.len()).then_some(index + 1)
    };
    let Some(neighbor) = neighbor else {
        return false;
    };
    items.swap(index, neighbor);
    true
}

fn remove_pending(
    queues: &mut std::collections::HashMap<String, std::collections::VecDeque<super::PendingSteer>>,
    task_id: &str,
    id: QueueItemId,
) -> bool {
    let Some(items) = queues.get_mut(task_id) else {
        return false;
    };
    let Some(index) = items.iter().position(|item| item.id == id) else {
        return false;
    };
    items.remove(index);
    if items.is_empty() {
        queues.remove(task_id);
    }
    true
}

fn update_pending(
    queues: &mut std::collections::HashMap<String, std::collections::VecDeque<super::PendingSteer>>,
    task_id: &str,
    id: QueueItemId,
    text: String,
    draft: InputDraft,
) -> bool {
    let Some(item) = queues
        .get_mut(task_id)
        .and_then(|items| items.iter_mut().find(|item| item.id == id))
    else {
        return false;
    };
    item.text = text;
    item.draft = draft;
    true
}
