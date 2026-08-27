//! Queue for messages typed while the agent is busy.

use std::borrow::Cow;

use maki_agent::AgentInput;
use maki_agent::{QueueDelivery, QueueItemId};

use super::{Action, App, Status, format_with_images};

use crate::agent::shared_queue::{QueueItem, QueueSender};
use crate::components::input::{InputAction, InputState, Submission};
use crate::components::queue_panel::QueueEntry;
use crate::input_document::InputDraft;
use crate::theme;

pub(crate) use crate::agent::shared_queue::QueuedMessage;

pub(crate) const EMPTY_PROMPT_ERR: &str = "prompt is empty";
pub(crate) const NO_QUEUE_ERR: &str = "session cannot queue messages";

pub(crate) enum SubmitOutcome {
    Started(Vec<Action>),
    Queued,
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

    pub(crate) fn text_messages(&self) -> Vec<String> {
        self.shared.as_ref().map_or(vec![], |s| s.text_messages())
    }

    pub(crate) fn begin_edit(&self, id: QueueItemId) -> Option<String> {
        self.shared.as_ref()?.begin_edit(id)
    }

    pub(crate) fn finish_edit(&self, id: QueueItemId, text: String) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|shared| shared.finish_edit(id, text))
    }

    pub(crate) fn cancel_edit(&self, id: QueueItemId) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|shared| shared.cancel_edit(id))
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
        let mut entries = self
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
            })
            .collect::<Vec<_>>();
        entries.extend(
            self.unsent_subagent_steers
                .get(task_id)
                .into_iter()
                .flatten()
                .map(|item| QueueEntry {
                    id: item.id,
                    text: Cow::Owned(item.text.clone()),
                    color: theme::current().foreground,
                    editable: true,
                    movable: true,
                }),
        );
        entries
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
        if self.queue_editor_active()
            || !self
                .active_queue_entries()
                .iter()
                .any(|entry| entry.editable)
        {
            return None;
        }
        if self.is_main_chat() {
            return Some(self.queue.delivery());
        }
        self.active_subagent_id()
            .and_then(|task_id| self.subagent_steers.get(task_id))
            .map(maki_agent::SteeringQueue::delivery)
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
        let count = self
            .active_queue_entries()
            .iter()
            .filter(|entry| entry.editable && !entry.movable)
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

    pub(super) fn pop_active_queue(&mut self) {
        if let Some(id) = self.active_queue_entries().first().map(|entry| entry.id) {
            self.delete_active_queue_item(id);
        }
    }

    pub(super) fn begin_queue_edit(&mut self, id: QueueItemId) {
        if self.queue_editor.is_some() {
            return;
        }
        let (target, draft) = if self.is_main_chat() {
            let Some(text) = self.queue.begin_edit(id) else {
                self.flash("Queued message was already sent".into());
                self.clamp_active_queue_focus();
                return;
            };
            (
                QueueTarget::Main,
                InputDraft {
                    text,
                    paste_ranges: Vec::new(),
                },
            )
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
            (QueueTarget::Task(task_id), draft)
        };
        let previous_input = self.active_input_box_mut().take_state();
        self.active_input_box_mut().set_draft(draft);
        self.active_input_box_mut().move_to_end();
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
            QueueTarget::Main => self.queue.finish_edit(editor.id, sub.text.clone()),
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
        if !self.queue_and_notify(QueuedMessage {
            text: item.text.clone(),
            images: Vec::new(),
        }) {
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
    pub(crate) fn submit_prompt(&mut self, msg: QueuedMessage) -> SubmitOutcome {
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
            if self.queue_and_notify(msg) {
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
        match self.submit_prompt(msg) {
            SubmitOutcome::Started(actions) => actions,
            SubmitOutcome::Queued => vec![],
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
        };
        let mut input = self.build_agent_input(&msg);
        input.preamble.push(maki_providers::Message::synthetic(
            maki_agent::goal_kickoff_message(condition),
        ));
        if self.status == Status::Streaming {
            let Some(ref shared) = self.queue.shared else {
                self.flash(NO_QUEUE_ERR.into());
                return vec![];
            };
            shared.push(QueueItem::Message {
                text: msg.text,
                image_count: 0,
                input,
                run_id: self.run_id,
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
    pub(super) fn queue_and_notify(&mut self, msg: QueuedMessage) -> bool {
        let Some(ref shared) = self.queue.shared else {
            return false;
        };
        let input = self.build_agent_input(&msg);
        shared.push(QueueItem::Message {
            text: msg.text,
            image_count: msg.images.len(),
            input,
            run_id: self.run_id,
            displayed: false,
        });
        true
    }

    /// Push restored queue items only here, never in `restore_display`: on
    /// load/rewind the display is restored before `respawn` swaps the shared
    /// queue, so pushing earlier would fill a queue that is about to die.
    pub(crate) fn flush_restored_queue(&mut self) {
        // The live queue owns the text from here on, so the recovery
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
        for text in self.state.session.meta.queued_messages.clone() {
            self.queue_and_notify(QueuedMessage {
                text,
                images: Vec::new(),
            });
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
    pub(super) fn on_queue_item_consumed(&mut self, text: &str, image_count: usize) {
        self.queue.clamp_focus();
        self.status = Status::Streaming;
        self.main_chat()
            .show_user_message(format_with_images(text, image_count));
    }

    pub(super) fn on_queue_batch_consumed(&mut self, items: &[maki_agent::QueueConsumedItem]) {
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
        preamble: Vec<maki_providers::Message>,
    ) -> Vec<Action> {
        let mut input = self.build_agent_input(&QueuedMessage {
            text: String::new(),
            images: Vec::new(),
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
        });
        input.preamble.push(maki_providers::Message::synthetic(
            maki_agent::goal_checkin_message(&goal.condition),
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
        if let Err(error) = self.snapshot_history_head() {
            self.flash(format!("Failed to snapshot workspace: {error}"));
            return Vec::new();
        }
        self.run_id += 1;
        self.goal_deferred = false;
        self.clear_exit_request();
        // New work supersedes text held for recovery after an agent error.
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        self.status = Status::Streaming;
        self.fire_session_autocmd("TurnStart", serde_json::json!({}));
        if !display.is_empty() {
            self.main_chat().show_user_message(display);
        }
        vec![Action::SendMessage(Box::new(input))]
    }
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
