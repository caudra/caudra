//! Queue for messages typed while the agent is busy.

use std::borrow::Cow;
use std::ops::Range;

use caudra_agent::{AgentInput, AgentMode};
use caudra_agent::{PromptAdmission, QueueDelivery, QueueItemId, is_run_failure_marker};
use caudra_providers::{HistoryItemKind, ImageMediaType, ImageSource};
use caudra_storage::id::CaudraId;

use super::{Action, App, Status};

use crate::agent::shared_queue::{QueueItem, QueueSender};
use crate::chat::format_with_images;
use crate::components::input::{InputAction, InputState, Submission};
use crate::components::mode_submission::{ModeSubmissionAction, ModeSubmissionChoice};
use crate::components::queue_panel::{QueueEntry, set_movement_flags};
use crate::input_document::InputDraft;
use crate::theme;

pub(crate) use crate::agent::shared_queue::QueuedMessage;

pub(crate) const ALREADY_SENT_ERR: &str = "Queued message was already sent";
pub(crate) const EMPTY_PROMPT_ERR: &str = "prompt is empty";
pub(crate) const NO_QUEUE_ERR: &str = "session cannot queue messages";
pub(crate) const REPLACE_BUSY_ERR: &str = "session is already stopping a run";
pub(crate) const CONTINUE_BUSY_ERR: &str = "session is already working";
pub(crate) const CONTINUE_EMPTY_ERR: &str = "nothing to continue";
pub(crate) const CONTINUE_HINT: &str = "/continue resumes the turn";
pub(crate) const PERMISSION_PUBLISH_ERR: &str = "Failed to initialize conversation permissions";
pub(crate) const MODE_DECISION_ERR: &str =
    "Plan submission needs a choice: keep editing, queue in Plan, or stop work and submit in Plan";
const MODE_DECISION_STALE: &str = "Session work changed; submit again to choose how to enter Plan";
const PLAN_TARGET_ERR: &str = "Plan target is unavailable; select Plan again before submitting";
const GOAL_SUBMISSION_ERR: &str = "Could not set the submitted goal";
pub(crate) const GOAL_SET: &str = "Goal set";
const MODE_DECISION_PENDING: &str = "Finish the pending Plan submission choice first";

pub(crate) enum SubmitOutcome {
    Started(Vec<Action>),
    Queued,
    Replacing(Vec<Action>),
    NeedsModeDecision(Box<PromptSubmission>),
    Rejected(&'static str),
}

pub(crate) struct PromptSubmission {
    input: Box<AgentInput>,
    text: String,
    paste_ranges: Vec<Range<usize>>,
    goal: Option<String>,
}

pub(super) struct PendingPlanSubmission {
    submission: PlanSubmissionSource,
    session: CaudraId,
    generation: Option<u64>,
}

enum PlanSubmissionSource {
    Draft(Box<PromptSubmission>),
    Queued(QueueItemId),
}

impl PromptSubmission {
    fn into_queue_item(self, run_id: u64, admission: PromptAdmission) -> QueueItem {
        QueueItem::Message {
            image_count: self.input.images.len(),
            text: self.text,
            paste_ranges: self.paste_ranges,
            input: self.input,
            run_id,
            admission,
            displayed: false,
        }
    }
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
    pub(super) fn wake_dispatch(&self) {
        if let Some(shared) = &self.shared {
            shared.wake_dispatch();
        }
    }
    pub(crate) fn set_shared(&mut self, shared: QueueSender) {
        self.shared = Some(shared);
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.shared.is_some()
    }

    pub(crate) fn is_processing(&self) -> bool {
        self.shared.as_ref().is_some_and(QueueSender::is_processing)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.shared.as_ref().is_none_or(|s| s.is_empty())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.len())
    }

    pub(crate) fn remove_id(&mut self, id: QueueItemId) -> bool {
        self.take_id(id).is_some()
    }

    pub(crate) fn take_id(&mut self, id: QueueItemId) -> Option<QueueItem> {
        let removed = self.shared.as_ref().and_then(|shared| shared.remove_id(id));
        if removed.is_some() {
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

    #[cfg(test)]
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
            .panel_entries()
            .len()
            .saturating_sub(crate::components::queue_panel::max_visible_entries());
        self.viewport = self
            .viewport
            .saturating_add_signed(-delta as isize)
            .min(max);
        let visible = crate::components::queue_panel::max_visible_entries();
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
            .saturating_sub(crate::components::queue_panel::max_visible_entries());
        self.viewport = self.viewport.min(max);
    }

    pub(crate) fn set_focus_at(&mut self, index: usize) {
        if let Some(id) = self.panel_entries().get(index).map(|entry| entry.id) {
            self.selected = Some(id);
            let visible = crate::components::queue_panel::max_visible_entries();
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
    pub(super) fn cancel_main_run(&mut self) -> u64 {
        let shared = self.queue.shared.clone();
        let _dispatch = shared.as_ref().map(QueueSender::lock_dispatch);
        self.stop_background_work();
        self.begin_main_cancel(
            false,
            self.status == Status::Streaming && self.queue.is_processing(),
        )
    }

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
            .saturating_sub(crate::components::queue_panel::max_visible_entries());
        self.task_queue_viewport = self
            .task_queue_viewport
            .saturating_add_signed(-delta as isize)
            .min(max);
        let visible = crate::components::queue_panel::max_visible_entries();
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
                self.status =
                    if self.cancelling_run.is_some() || !self.queue.panel_entries().is_empty() {
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
        if let Some(id) = self.focused_queue_entry().map(|entry| entry.id) {
            self.delete_active_queue_item(id);
        }
    }

    pub(super) fn focused_queue_entry(&self) -> Option<QueueEntry<'static>> {
        self.active_queue_entries()
            .into_iter()
            .nth(self.active_queue_focus().unwrap_or(0))
    }

    pub(super) fn set_focused_queue_admission(&mut self, admission: PromptAdmission) {
        if let Some(id) = self.focused_queue_entry().map(|entry| entry.id) {
            self.set_queue_admission(id, admission);
        }
    }

    /// Only the main queue has lanes to move between, and only a prompt that
    /// is still waiting can change the one it waits in.
    pub(super) fn set_queue_admission(&mut self, id: QueueItemId, admission: PromptAdmission) {
        if !self.is_main_chat() || !self.is_lane_changeable(id) {
            return;
        }
        if self.queue.set_admission(id, admission) {
            self.queue.select(id);
            self.flash(match admission {
                PromptAdmission::Queue => "Prompt moved to Up next".into(),
                PromptAdmission::Steer => "Prompt will guide the current run".into(),
                PromptAdmission::Interrupt => return,
            });
        }
    }

    fn is_lane_changeable(&self, id: QueueItemId) -> bool {
        self.active_queue_entries().iter().any(|entry| {
            entry.id == id
                && matches!(
                    entry.admission,
                    Some(PromptAdmission::Queue | PromptAdmission::Steer)
                )
        })
    }

    pub(super) fn open_focused_queue_actions(&mut self) {
        if let Some(id) = self.focused_queue_entry().map(|entry| entry.id) {
            self.open_queue_actions(id);
        }
    }

    pub(super) fn open_queue_actions(&mut self, id: QueueItemId) {
        let entries = self.active_queue_entries();
        let Some(entry) = entries.iter().find(|entry| entry.id == id) else {
            return;
        };
        let main_queue = self.is_main_chat();
        self.select_active_queue_item(id);
        self.queue_actions.open(entry, main_queue);
    }

    pub(super) fn replace_with_focused_queue_item(&mut self) -> Vec<Action> {
        let Some(id) = self.focused_queue_entry().map(|entry| entry.id) else {
            return Vec::new();
        };
        self.replace_with_queued_item(id)
    }

    /// Takes the prompt out of the queue and applies it through the shared
    /// replacement path, so it cancels the running turn exactly the way a
    /// freshly typed replacement does. A refusal puts it back where it was.
    pub(super) fn replace_with_queued_item(&mut self, id: QueueItemId) -> Vec<Action> {
        if !self.is_main_chat() || !self.is_lane_changeable(id) {
            return Vec::new();
        }
        if self
            .queue
            .shared
            .as_ref()
            .and_then(|queue| queue.input_mode(id))
            .is_some_and(|mode| self.plan_mode_conflicts(&mode))
        {
            self.pending_plan_submission = Some(PendingPlanSubmission {
                submission: PlanSubmissionSource::Queued(id),
                session: self.state.session.id,
                generation: self
                    .background
                    .as_ref()
                    .map(|background| background.generation()),
            });
            self.mode_submission.open();
            return Vec::new();
        }
        self.replace_queued_submission(id)
    }

    fn replace_queued_submission(&mut self, id: QueueItemId) -> Vec<Action> {
        if self.cancelling_run.is_some() && self.replacement_item.is_none() {
            self.flash(REPLACE_BUSY_ERR.into());
            return Vec::new();
        }
        let Some(QueueItem::Message {
            text,
            paste_ranges,
            input,
            admission,
            ..
        }) = self.queue.take_id(id)
        else {
            self.flash(ALREADY_SENT_ERR.into());
            return Vec::new();
        };
        let submission = PromptSubmission {
            text,
            paste_ranges,
            input,
            goal: None,
        };
        if let Some(error) = self.submission_error(&submission) {
            self.queue_submission(submission, admission);
            self.flash(error.into());
            return Vec::new();
        }
        let outcome = self.submit_prepared(submission, PromptAdmission::Interrupt);
        self.handle_submit_outcome(outcome)
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
                self.flash(ALREADY_SENT_ERR.into());
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
                self.flash(ALREADY_SENT_ERR.into());
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
            self.flash(ALREADY_SENT_ERR.into());
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
                mentions: Vec::new(),
                commits: Vec::new(),
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
        let visible = crate::components::queue_panel::max_visible_entries();
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
            .saturating_sub(crate::components::queue_panel::max_visible_entries());
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
        let submission = self.prepare_submission(msg);
        self.admit_submission(submission, admission)
    }

    fn prepare_submission(&self, msg: QueuedMessage) -> PromptSubmission {
        PromptSubmission {
            input: Box::new(self.build_agent_input(&msg)),
            text: msg.text,
            paste_ranges: msg.paste_ranges,
            goal: None,
        }
    }

    pub(super) fn plan_submission_conflicts(&self, input: &AgentInput) -> bool {
        self.plan_mode_conflicts(&input.mode)
    }

    fn plan_mode_conflicts(&self, mode: &AgentMode) -> bool {
        mode.is_planning()
            && !self.execution_agent_mode().is_planning()
            && (self.status == Status::Streaming
                || self.has_session_work()
                || self.queue.is_processing())
    }

    fn submission_error(&self, submission: &PromptSubmission) -> Option<&'static str> {
        if self.pending_plan_submission.is_some() {
            return Some(MODE_DECISION_PENDING);
        }
        if let Some(error) = self.background_delivery.fence.admission_error() {
            return Some(error);
        }
        if let Some(reason) = self.sandbox_network_dispatch_blocker() {
            return Some(reason);
        }
        if submission.input.message.trim().is_empty()
            && submission.input.images.is_empty()
            && !submission.input.resume
            && submission.input.prompt.is_none()
        {
            return Some(EMPTY_PROMPT_ERR);
        }
        if submission.input.mode.is_read_only() && self.state.mode == super::Mode::Plan {
            return Some(PLAN_TARGET_ERR);
        }
        if submission.input.mode.is_planning()
            && submission.input.mode != self.agent_mode_for(super::Mode::Plan)
        {
            return Some(PLAN_TARGET_ERR);
        }
        None
    }

    fn admit_submission(
        &mut self,
        submission: PromptSubmission,
        admission: PromptAdmission,
    ) -> SubmitOutcome {
        if let Some(error) = self.submission_error(&submission) {
            return SubmitOutcome::Rejected(error);
        }
        if self.plan_submission_conflicts(&submission.input) {
            return SubmitOutcome::NeedsModeDecision(Box::new(submission));
        }
        self.submit_prepared(submission, admission)
    }

    fn submit_prepared(
        &mut self,
        submission: PromptSubmission,
        admission: PromptAdmission,
    ) -> SubmitOutcome {
        if let Some(error) = self.submission_error(&submission) {
            return SubmitOutcome::Rejected(error);
        }
        let deferred = self.status == Status::Streaming
            || (self.status == Status::Idle
                && (self.has_session_work()
                    || self.queue.is_processing()
                    || !self.queue.is_empty()));
        if deferred && !self.queue.is_connected() {
            return SubmitOutcome::Rejected(NO_QUEUE_ERR);
        }
        if self.cancelling_run.is_some()
            && (!deferred
                || (admission == PromptAdmission::Interrupt && self.replacement_item.is_none()))
        {
            return SubmitOutcome::Rejected(REPLACE_BUSY_ERR);
        }
        if submission.goal.is_some()
            && !deferred
            && let Err(error) = self.publish_conversation_permissions()
        {
            tracing::warn!(%error, "failed to initialize goal conversation permissions");
            return SubmitOutcome::Rejected(PERMISSION_PUBLISH_ERR);
        }
        if let Some(goal) = &submission.goal {
            if self.state.goal.set(goal).is_err() {
                return SubmitOutcome::Rejected(GOAL_SUBMISSION_ERR);
            }
            self.flash(GOAL_SET.into());
        }
        if deferred {
            if admission == PromptAdmission::Interrupt {
                return self.replace_submission(submission);
            }
            if self.queue_submission(submission, admission) {
                SubmitOutcome::Queued
            } else {
                SubmitOutcome::Rejected(NO_QUEUE_ERR)
            }
        } else {
            let display = if submission.input.resume {
                String::new()
            } else {
                format_with_images(&submission.text, submission.input.images.len())
            };
            SubmitOutcome::Started(self.start_run(*submission.input, display))
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
        let outcome = self.submit_prompt_with_admission(msg, admission);
        self.handle_submit_outcome(outcome)
    }

    pub(super) fn handle_submit_outcome(&mut self, outcome: SubmitOutcome) -> Vec<Action> {
        match outcome {
            SubmitOutcome::Started(actions) => actions,
            SubmitOutcome::Queued => vec![],
            SubmitOutcome::Replacing(actions) => actions,
            SubmitOutcome::NeedsModeDecision(submission) => {
                let text = if let Some(goal) = &submission.goal {
                    format!("/goal {goal}")
                } else if submission.input.resume {
                    "/continue".into()
                } else {
                    submission.text.clone()
                };
                self.input_box.set_state(InputState::new(
                    InputDraft {
                        text,
                        paste_ranges: submission.paste_ranges.clone(),
                    },
                    submission.input.images.clone(),
                ));
                self.pending_plan_submission = Some(PendingPlanSubmission {
                    submission: PlanSubmissionSource::Draft(submission),
                    session: self.state.session.id,
                    generation: self
                        .background
                        .as_ref()
                        .map(|background| background.generation()),
                });
                self.mode_submission.open();
                Vec::new()
            }
            SubmitOutcome::Rejected(e) => {
                self.flash(e.into());
                vec![]
            }
        }
    }

    pub(super) fn submit_explicit_input(&mut self, input: AgentInput, text: String) -> Vec<Action> {
        let outcome = self.admit_submission(
            PromptSubmission {
                input: Box::new(input),
                text,
                paste_ranges: Vec::new(),
                goal: None,
            },
            PromptAdmission::Queue,
        );
        self.handle_submit_outcome(outcome)
    }

    pub(super) fn handle_mode_submission(&mut self, action: ModeSubmissionAction) -> Vec<Action> {
        let choice = match action {
            ModeSubmissionAction::Consumed => return Vec::new(),
            ModeSubmissionAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                return Vec::new();
            }
            ModeSubmissionAction::Close => ModeSubmissionChoice::KeepEditing,
            ModeSubmissionAction::Select(choice) => choice,
        };
        self.mode_submission.close();
        let Some(pending) = self.pending_plan_submission.take() else {
            return Vec::new();
        };
        if matches!(choice, ModeSubmissionChoice::KeepEditing) {
            return Vec::new();
        }
        if pending.session != self.state.session.id
            || pending.generation
                != self
                    .background
                    .as_ref()
                    .map(|background| background.generation())
        {
            self.flash(MODE_DECISION_STALE.into());
            return Vec::new();
        }
        let admission = match choice {
            ModeSubmissionChoice::Queue => PromptAdmission::Queue,
            ModeSubmissionChoice::Stop => PromptAdmission::Interrupt,
            ModeSubmissionChoice::KeepEditing => unreachable!(),
        };
        let submission = match pending.submission {
            PlanSubmissionSource::Draft(submission) => submission,
            PlanSubmissionSource::Queued(id) => {
                if matches!(choice, ModeSubmissionChoice::Queue) {
                    self.queue.set_admission(id, PromptAdmission::Queue);
                    return Vec::new();
                }
                return self.replace_queued_submission(id);
            }
        };
        let outcome = self.submit_prepared(*submission, admission);
        if !matches!(outcome, SubmitOutcome::Rejected(_)) {
            self.input_box.discard();
        }
        self.handle_submit_outcome(outcome)
    }

    pub(super) fn submit_goal(&mut self, condition: &str) -> Vec<Action> {
        let msg = QueuedMessage {
            text: condition.to_owned(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            paste_ranges: Vec::new(),
        };
        let mut submission = self.prepare_submission(msg);
        submission.goal = Some(condition.into());
        submission
            .input
            .preamble
            .push(caudra_providers::Message::synthetic(
                caudra_agent::goal_kickoff_message(condition),
            ));
        let outcome = self.admit_submission(submission, PromptAdmission::Queue);
        self.handle_submit_outcome(outcome)
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
        let submission = self.prepare_submission(msg);
        self.queue_submission(submission, admission)
    }

    fn queue_submission(
        &mut self,
        submission: PromptSubmission,
        admission: PromptAdmission,
    ) -> bool {
        let Some(shared) = self.queue.shared.clone() else {
            return false;
        };
        if self.automatic_wakes_suppressed {
            self.rearm_background();
        }
        shared.push(submission.into_queue_item(self.run_id, admission));
        true
    }

    fn replace_submission(&mut self, submission: PromptSubmission) -> SubmitOutcome {
        let Some(shared) = self.queue.shared.clone() else {
            return SubmitOutcome::Rejected(NO_QUEUE_ERR);
        };
        let mut replacement = submission.into_queue_item(self.run_id, PromptAdmission::Interrupt);
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
        self.stop_background_work();
        let (id, active) = shared.replace(cancelled_run, replacement_run, replacement);
        let cancelled_run = self.begin_main_cancel(true, active);
        debug_assert_eq!(self.run_id, replacement_run);
        self.replacement_item = Some(id);
        SubmitOutcome::Replacing(vec![Action::CancelAgent {
            run_id: cancelled_run,
        }])
    }

    /// Push restored queue items only here, never in `restore_display`: on
    /// load/rewind the display is restored before `respawn` swaps the shared
    /// queue, so pushing earlier would fill a queue that is about to die.
    pub(crate) fn flush_restored_queue(&mut self) {
        // The live queue owns the prompts from here on, so the recovery
        // snapshot must stop overriding it on save.
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        let Some(shared) = self.queue.shared.clone() else {
            return;
        };
        let delivery = if self.state.session.meta.queued_messages_together {
            QueueDelivery::TogetherNextTurn
        } else {
            QueueDelivery::Separate
        };
        let mut entries = Vec::with_capacity(self.state.session.meta.queued_messages.len());
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
            let mode = prompt.mode.map_or(self.state.mode, Into::into);
            if mode == super::Mode::Plan && matches!(self.state.plan, super::mode::PlanState::None)
            {
                let selected = self.state.mode;
                self.enter_plan();
                self.state.mode = selected;
            }
            let mut submission = self.prepare_submission(QueuedMessage {
                mentions: self.scan_mentions(&prompt.text),
                commits: self.scan_commits(&prompt.text),
                text: prompt.text,
                images,
                paste_ranges: prompt
                    .paste_ranges
                    .into_iter()
                    .map(|range| range.start..range.end)
                    .collect(),
            });
            submission.input.mode = self.agent_mode_for(mode);
            entries.push(submission.into_queue_item(self.run_id, admission));
        }
        if !entries.is_empty() && self.automatic_wakes_suppressed {
            self.rearm_background();
        }
        let ready = self.status == Status::Idle
            && !self.awaiting_input()
            && !self.has_session_work()
            && !self.automatic_wakes_suppressed
            && self.shell.active_ids().is_empty();
        if ready && !entries.is_empty() {
            self.status = Status::Streaming;
        }
        shared.restore_pending(entries, delivery, ready);
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
            self.rearm_background();
        }
        self.goal_deferred = false;
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
            self.rearm_background();
        }
        self.goal_deferred = false;
        self.queue.clamp_focus();
        self.status = Status::Streaming;
        let messages = items
            .iter()
            .map(|item| format_with_images(&item.text, item.image_count));
        self.main_chat().show_user_messages(messages);
    }

    /// Immediate path: kick off the agent and draw the bubble in the same
    /// frame, so the user sees their message land where it will stay.
    #[cfg(test)]
    pub(super) fn start_from_queue(&mut self, msg: &QueuedMessage) -> Vec<Action> {
        let display = format_with_images(&msg.text, msg.images.len());
        let input = self.build_agent_input(msg);
        self.start_run(input, display)
    }

    pub(crate) fn start_mailbox_run(
        &mut self,
        preamble: Vec<caudra_providers::Message>,
    ) -> Vec<Action> {
        if self.automatic_wakes_suppressed || self.status != Status::Idle {
            return Vec::new();
        }
        let mut input = self.continuation_input();
        input.preamble = preamble;
        self.start_run(input, String::new())
    }

    /// Whether the last run died mid-turn and left a transcript a resume can pick back up. The
    /// agent closes such a run on a marker, so the marker at the tail is the signal; an error
    /// that landed before the turn wrote anything leaves none and there is nothing to offer.
    pub(super) fn died_mid_turn(&self) -> bool {
        self.shared_history.as_ref().is_some_and(|history| {
            matches!(
                history.load().messages.last().map(|item| &item.kind),
                Some(HistoryItemKind::User { text, .. }) if is_run_failure_marker(text)
            )
        })
    }

    /// Resumes with no turn of its own, so nothing the user did not type
    /// reaches the transcript. What the request tail still needs is the
    /// agent's call, since it is the only side that owns history.
    pub(super) fn continue_run(&mut self) -> Vec<Action> {
        if self.cancelling_run.is_some()
            || (self.status == Status::Streaming
                && !(self.state.mode == super::Mode::Plan
                    && !self.execution_agent_mode().is_planning()))
        {
            self.flash(CONTINUE_BUSY_ERR.into());
            return Vec::new();
        }
        if self
            .shared_history
            .as_ref()
            .is_none_or(|history| history.load().messages.is_empty())
        {
            self.flash(CONTINUE_EMPTY_ERR.into());
            return Vec::new();
        }
        let mut input = self.build_agent_input(&QueuedMessage {
            text: String::new(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            paste_ranges: Vec::new(),
        });
        input.resume = true;
        self.submit_explicit_input(input, String::new())
    }

    pub(crate) fn start_goal_checkin(&mut self) -> Vec<Action> {
        let Some(goal) = self.state.goal.snapshot() else {
            self.goal_deferred = false;
            return vec![];
        };
        let mut input = self.continuation_input();
        input.preamble.push(caudra_providers::Message::synthetic(
            caudra_agent::goal_checkin_message(&goal.condition),
        ));
        self.start_run(input, String::new())
    }

    /// The one place a fresh run starts: every path that emits
    /// `Action::SendMessage` must go through here so `run_id` bumps exactly
    /// once per run.
    pub(super) fn start_run(&mut self, input: AgentInput, display: String) -> Vec<Action> {
        if let Err(error) = self.admit_run() {
            self.flash(error);
            return Vec::new();
        }
        self.start_admitted_run(input, display)
    }

    pub(crate) fn check_run_admission(&self) -> Result<(), String> {
        if let Some(reason) = self.sandbox_network_dispatch_blocker() {
            return Err(reason.into());
        }
        if self.cancelling_run.is_some() {
            return Err(super::REVERT_BUSY_MSG.into());
        }
        Ok(())
    }

    pub(crate) fn admit_run(&mut self) -> Result<(), String> {
        self.check_run_admission()?;
        // The turn is what earns this session its row, and a conversation
        // grant cannot be published before there is one.
        self.publish_conversation_permissions()
            .map_err(|error| format!("{PERMISSION_PUBLISH_ERR}: {error}"))
    }

    pub(super) fn start_admitted_run(&mut self, input: AgentInput, display: String) -> Vec<Action> {
        self.run_id += 1;
        self.background_delivery.invalidate();
        if !input.message.is_empty() || !input.images.is_empty() || input.resume {
            self.rearm_background();
        }
        self.goal_deferred = false;
        self.clear_exit_request();
        // New work supersedes text held for recovery after an agent error.
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        // Streaming from here, so the spinner runs and a second submit queues
        // instead of racing a second run.
        self.status = Status::Streaming;
        // No workspace capture here: the agent takes one behind the first tool
        // call that could change a file, so a turn that only reads or only
        // talks costs nothing and the run starts without waiting on a walk.
        self.fire_session_autocmd("TurnStart", serde_json::json!({}));
        if !display.is_empty() {
            self.main_chat().show_user_message(display);
        }
        vec![Action::SendMessage(Box::new(input))]
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
