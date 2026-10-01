//! Queue of work handed from the UI to the agent loop.
//!
//! Shutdown rides on `Drop`: when the last [`QueueSender`] goes away, flume
//! closes the notify channel, so the receiver's `recv_notify` wakes with an
//! `Err` and the agent loop falls out of its main loop on its own. That way
//! nobody needs a separate "please stop" flag, and callers can't forget to
//! set it.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use caudra_agent::{
    AgentInput, AgentMode, CommitRef, EditableQueue, EditableQueueReceiver, ExtractedCommand,
    ImageSource, InterruptSource, Mention, PromptAdmission, QueueDelivery, QueueItemId,
    QueuedInterrupt, editable_queue,
};
use caudra_storage::sessions::StoredMode;

use crate::components::input::{InputState, Submission};
use crate::components::queue_panel::QueueEntry;
use crate::input_document::InputDraft;
use crate::theme;

const COMPACT_LABEL: &str = "/compact";

pub(crate) struct QueuedMessage {
    pub(crate) text: String,
    pub(crate) images: Vec<ImageSource>,
    pub(crate) mentions: Vec<Mention>,
    pub(crate) commits: Vec<CommitRef>,
    pub(crate) paste_ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingPrompt {
    pub(crate) text: String,
    pub(crate) images: Vec<ImageSource>,
    pub(crate) paste_ranges: Vec<Range<usize>>,
    pub(crate) admission: PromptAdmission,
    pub(crate) mode: Option<StoredMode>,
}

impl From<Submission> for QueuedMessage {
    fn from(sub: Submission) -> Self {
        let trim_start = sub.draft.text.len() - sub.draft.text.trim_start().len();
        let trim_end = trim_start + sub.text.len();
        let paste_ranges = sub
            .draft
            .paste_ranges
            .into_iter()
            .filter_map(|range| {
                let start = range.start.max(trim_start);
                let end = range.end.min(trim_end);
                (start < end).then(|| start - trim_start..end - trim_start)
            })
            .collect();
        Self {
            text: sub.text,
            images: sub.images,
            mentions: sub.mentions,
            commits: sub.commits,
            paste_ranges,
        }
    }
}

pub(crate) enum QueueItem {
    Message {
        text: String,
        image_count: usize,
        paste_ranges: Vec<Range<usize>>,
        /// Boxed to keep the queue's cheap `Compact` variant from carrying a
        /// whole agent request's worth of padding.
        input: Box<AgentInput>,
        run_id: u64,
        admission: PromptAdmission,
        /// `true` when the UI already drew the bubble (immediate dispatch).
        /// The agent then skips `QueueItemConsumed` so we don't draw it twice.
        /// `false` when the user typed while the agent was busy: the UI waits
        /// for `QueueItemConsumed` before drawing.
        displayed: bool,
    },
    Compact {
        run_id: u64,
    },
}

#[derive(PartialEq, Eq)]
enum MovementLane {
    Visible(PromptAdmission),
    Hidden,
}

impl QueueItem {
    pub(crate) fn run_id(&self) -> u64 {
        match self {
            Self::Message { run_id, .. } | Self::Compact { run_id } => *run_id,
        }
    }

    fn set_run_id(&mut self, next: u64) {
        match self {
            Self::Message { run_id, .. } | Self::Compact { run_id } => *run_id = next,
        }
    }

    fn admission(&self) -> PromptAdmission {
        match self {
            Self::Message { admission, .. } => *admission,
            Self::Compact { .. } => PromptAdmission::Queue,
        }
    }

    fn movement_lane(&self) -> Option<MovementLane> {
        match self {
            Self::Message {
                admission,
                displayed: false,
                ..
            } => Some(MovementLane::Visible(*admission)),
            Self::Message {
                displayed: true, ..
            } => Some(MovementLane::Hidden),
            Self::Compact { .. } => None,
        }
    }

    fn mode(&self) -> Option<&AgentMode> {
        match self {
            Self::Message { input, .. } => Some(&input.mode),
            Self::Compact { .. } => None,
        }
    }

    fn guide_ready(&self, next_ready: bool, execution_mode: &AgentMode) -> bool {
        self.admission() == PromptAdmission::Steer
            && (next_ready || self.mode() == Some(execution_mode))
    }

    fn as_queue_entry(&self, id: QueueItemId) -> QueueEntry<'static> {
        match self {
            Self::Message {
                text, admission, ..
            } => QueueEntry {
                id,
                text: Cow::Owned(text.clone()),
                color: theme::current().foreground,
                editable: true,
                movable: false,
                can_move_up: false,
                can_move_down: false,
                admission: Some(*admission),
            },
            Self::Compact { .. } => QueueEntry {
                id,
                text: Cow::Borrowed(COMPACT_LABEL),
                color: theme::current()
                    .queue
                    .fg
                    .unwrap_or(theme::current().foreground),
                editable: false,
                movable: false,
                can_move_up: false,
                can_move_down: false,
                admission: None,
            },
        }
    }

    fn into_extracted_command(self, id: QueueItemId) -> ExtractedCommand {
        match self {
            Self::Message { input, run_id, .. } => ExtractedCommand::Interrupt(*input, run_id, id),
            Self::Compact { run_id } => ExtractedCommand::Compact(run_id),
        }
    }

    /// Immediate-dispatch messages already sit in the chat, so hiding them
    /// here stops the panel from reserving a row the agent is about to free,
    /// which used to make the bubble hop up by one frame.
    fn visible_in_panel(&self) -> bool {
        match self {
            Self::Message { displayed, .. } => !displayed,
            Self::Compact { .. } => true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct QueueSender {
    dispatch_guard: SharedDispatchGuard,
    next_ready: Arc<AtomicBool>,
    queue: EditableQueue<QueueItem>,
    paused: Arc<AtomicBool>,
    claim_gate: Arc<Mutex<()>>,
    active: Arc<AtomicBool>,
    active_run_id: Arc<AtomicU64>,
    execution_mode: Arc<Mutex<AgentMode>>,
    processing: Arc<AtomicBool>,
}

pub(crate) struct QueueReceiver {
    dispatch_guard: SharedDispatchGuard,
    next_ready: Arc<AtomicBool>,
    queue: EditableQueueReceiver<QueueItem>,
    paused: Arc<AtomicBool>,
    claim_gate: Arc<Mutex<()>>,
    active: Arc<AtomicBool>,
    active_run_id: Arc<AtomicU64>,
    execution_mode: Arc<Mutex<AgentMode>>,
    processing: Arc<AtomicBool>,
}

type SharedDispatchGuard = Arc<Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>>;

pub(crate) fn queue() -> (QueueSender, QueueReceiver) {
    let (queue, receiver) = editable_queue();
    let paused = Arc::new(AtomicBool::new(false));
    let claim_gate = Arc::new(Mutex::new(()));
    let active = Arc::new(AtomicBool::new(false));
    let active_run_id = Arc::new(AtomicU64::new(0));
    let execution_mode = Arc::new(Mutex::new(AgentMode::Build));
    let processing = Arc::new(AtomicBool::new(false));
    let dispatch_guard = SharedDispatchGuard::default();
    let next_ready = Arc::new(AtomicBool::new(true));
    (
        QueueSender {
            dispatch_guard: dispatch_guard.clone(),
            next_ready: Arc::clone(&next_ready),
            queue,
            paused: Arc::clone(&paused),
            claim_gate: Arc::clone(&claim_gate),
            active: Arc::clone(&active),
            active_run_id: Arc::clone(&active_run_id),
            execution_mode: Arc::clone(&execution_mode),
            processing: Arc::clone(&processing),
        },
        QueueReceiver {
            dispatch_guard,
            next_ready,
            queue: receiver,
            paused,
            claim_gate,
            active,
            active_run_id,
            execution_mode,
            processing,
        },
    )
}

impl QueueSender {
    pub(crate) fn lock_dispatch(&self) -> MutexGuard<'_, ()> {
        self.claim_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(crate) fn wake_dispatch(&self) {
        self.queue.wake();
    }

    pub(crate) fn allow_next_turn(&self, ready: bool) {
        let _claim = self.lock_dispatch();
        let ready = ready && !self.is_processing();
        if self.next_ready.swap(ready, Ordering::AcqRel) != ready && ready {
            self.queue.wake();
        }
    }

    pub(crate) fn has_priority_input(&self) -> bool {
        let execution_mode = self
            .execution_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next_ready = self.next_ready.load(Ordering::Acquire);
        self.queue.has_matching(|item| {
            matches!(
                item,
                QueueItem::Message {
                    displayed: true,
                    ..
                }
            ) || item.admission() == PromptAdmission::Interrupt
                || item.guide_ready(next_ready, &execution_mode)
        })
    }

    pub(crate) fn set_execution_mode(&self, mode: AgentMode) {
        let _claim = self.lock_dispatch();
        *self
            .execution_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
        self.queue.wake();
    }

    pub(crate) fn set_dispatch_guard(&self, guard: Arc<dyn Fn() -> bool + Send + Sync>) {
        if let Ok(mut slot) = self.dispatch_guard.lock() {
            *slot = Some(guard);
        }
    }

    pub(crate) fn push(&self, entry: QueueItem) -> QueueItemId {
        if matches!(
            entry,
            QueueItem::Message {
                displayed: true,
                ..
            }
        ) {
            let run_id = entry.run_id();
            self.queue.retain_mut_and_push(entry, |item| {
                if item.run_id() < run_id {
                    item.set_run_id(run_id);
                }
                true
            })
        } else {
            self.queue.push(entry)
        }
    }

    pub(crate) fn restore_pending(
        &self,
        entries: impl IntoIterator<Item = QueueItem>,
        delivery: QueueDelivery,
        ready: bool,
    ) {
        let _claim = self.lock_dispatch();
        self.next_ready
            .store(ready && !self.is_processing(), Ordering::Release);
        self.queue.set_delivery(delivery);
        for entry in entries {
            self.queue.push(entry);
        }
        self.queue.wake();
    }

    pub(crate) fn remove_id(&self, id: QueueItemId) -> Option<QueueItem> {
        self.queue.remove_with_delivery_guard(id, |item| {
            matches!(
                item,
                QueueItem::Message {
                    admission: PromptAdmission::Queue,
                    displayed: false,
                    ..
                }
            )
        })
    }

    pub(crate) fn input_mode(&self, id: QueueItemId) -> Option<AgentMode> {
        self.queue
            .entries(|candidate, item, _| {
                if candidate != id {
                    return None;
                }
                match item {
                    QueueItem::Message { input, .. } => Some(input.mode.clone()),
                    QueueItem::Compact { .. } => None,
                }
            })
            .into_iter()
            .flatten()
            .next()
    }

    pub(crate) fn begin_edit(&self, id: QueueItemId) -> Option<InputState> {
        self.queue.begin_edit(id, |item| match item {
            QueueItem::Message {
                text,
                paste_ranges,
                input,
                ..
            } => Some(InputState::new(
                InputDraft {
                    text: text.clone(),
                    paste_ranges: paste_ranges.clone(),
                },
                input.images.clone(),
            )),
            QueueItem::Compact { .. } => None,
        })
    }

    pub(crate) fn finish_edit(&self, id: QueueItemId, message: QueuedMessage) -> bool {
        self.queue.finish_edit(id, move |item| {
            if let QueueItem::Message {
                text: display,
                image_count,
                paste_ranges,
                input,
                ..
            } = item
            {
                display.clone_from(&message.text);
                *image_count = message.images.len();
                paste_ranges.clone_from(&message.paste_ranges);
                input.message = message.text;
                input.images = message.images;
            }
        })
    }

    pub(crate) fn cancel_edit(&self, id: QueueItemId) -> bool {
        self.queue.cancel_edit(id)
    }

    pub(crate) fn set_admission(&self, id: QueueItemId, admission: PromptAdmission) -> bool {
        self.queue.update_with_delivery_guard(
            id,
            |item| {
                if let QueueItem::Message {
                    admission: current, ..
                } = item
                {
                    *current = admission;
                }
            },
            |item| {
                matches!(
                    item,
                    QueueItem::Message {
                        admission: PromptAdmission::Queue,
                        displayed: false,
                        ..
                    }
                )
            },
        )
    }

    pub(crate) fn move_up(&self, id: QueueItemId) -> bool {
        self.queue.move_up_by(id, QueueItem::movement_lane)
    }

    pub(crate) fn move_down(&self, id: QueueItemId) -> bool {
        self.queue.move_down_by(id, QueueItem::movement_lane)
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn is_processing(&self) -> bool {
        self.processing.load(Ordering::Acquire)
    }

    pub(crate) fn clear(&self) {
        self.queue.clear();
        self.paused.store(false, Ordering::Release);
    }

    pub(crate) fn replace(
        &self,
        cancelled_run_id: u64,
        run_id: u64,
        mut replacement: QueueItem,
    ) -> (QueueItemId, bool) {
        let _claim = self
            .claim_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = self.active.load(Ordering::Acquire)
            && self.active_run_id.load(Ordering::Relaxed) == cancelled_run_id;
        replacement.set_run_id(run_id);
        let id = self.queue.retain_mut_and_push(replacement, |item| {
            if item.admission() == PromptAdmission::Interrupt {
                return false;
            }
            if matches!(
                item,
                QueueItem::Message {
                    run_id,
                    displayed: true,
                    ..
                } if *run_id == cancelled_run_id
            ) {
                return false;
            }
            item.set_run_id(run_id);
            true
        });
        (id, active)
    }

    pub(crate) fn update_pending_replacement(
        &self,
        run_id: u64,
        replacement: QueueItem,
    ) -> Result<QueueItemId, Box<QueueItem>> {
        let _claim = self
            .claim_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self
            .queue
            .has_matching(|item| item.admission() == PromptAdmission::Interrupt)
        {
            return Err(Box::new(replacement));
        }
        Ok(self.queue.retain_mut_and_push(replacement, |item| {
            if item.admission() == PromptAdmission::Interrupt {
                return false;
            }
            item.set_run_id(run_id);
            true
        }))
    }

    pub(crate) fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.queue.wake();
    }

    pub(crate) fn delivery(&self) -> QueueDelivery {
        self.queue.delivery()
    }

    #[cfg(test)]
    pub(crate) fn set_delivery(&self, delivery: QueueDelivery) {
        self.queue.set_delivery(delivery);
    }

    pub(crate) fn toggle_delivery(&self) -> QueueDelivery {
        self.queue.toggle_delivery()
    }

    #[cfg(test)]
    pub(crate) fn text_messages(&self) -> Vec<String> {
        self.pending_prompts()
            .into_iter()
            .map(|prompt| prompt.text)
            .collect()
    }

    pub(crate) fn pending_prompts(&self) -> Vec<PendingPrompt> {
        self.queue
            .entries(|_, item, _| match item {
                QueueItem::Message {
                    text,
                    paste_ranges,
                    input,
                    admission,
                    displayed: false,
                    ..
                } => Some(PendingPrompt {
                    text: text.clone(),
                    images: input.images.clone(),
                    paste_ranges: paste_ranges.clone(),
                    admission: *admission,
                    mode: Some(if input.mode.is_planning() {
                        StoredMode::Plan
                    } else {
                        StoredMode::Build
                    }),
                }),
                QueueItem::Message { .. } | QueueItem::Compact { .. } => None,
            })
            .into_iter()
            .flatten()
            .collect()
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        let mut entries = self
            .queue
            .entries(|id, item, _| item.visible_in_panel().then(|| item.as_queue_entry(id)))
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        crate::components::queue_panel::set_movement_flags(&mut entries);
        entries.sort_by_key(|entry| match entry.admission {
            Some(PromptAdmission::Interrupt) => 0,
            Some(PromptAdmission::Steer) => 1,
            Some(PromptAdmission::Queue) | None => 2,
        });
        entries
    }
}

impl QueueReceiver {
    pub(crate) fn hold_next_turn(&self) {
        self.next_ready.store(false, Ordering::Release);
    }

    fn dispatch_allowed(&self) -> bool {
        !crate::sandbox::transfer::active()
            && self
                .dispatch_guard
                .lock()
                .is_ok_and(|guard| guard.as_ref().is_none_or(|guard| guard()))
    }

    pub(crate) fn claim_idle(&self, min_run_id: u64) -> Vec<(QueueItemId, QueueItem)> {
        let _claim = self
            .claim_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.paused.load(Ordering::Acquire) || !self.dispatch_allowed() {
            return Vec::new();
        }
        self.processing.store(true, Ordering::Release);
        let next_ready = self.next_ready.load(Ordering::Acquire);
        let mut execution_mode = self
            .execution_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let immediate = self.queue.claim_one_matching(|item| {
                matches!(
                    item,
                    QueueItem::Message {
                        displayed: true,
                        ..
                    }
                )
            });
            let claimed = if !immediate.is_empty() {
                immediate
            } else if self
                .queue
                .has_matching(|item| item.admission() == PromptAdmission::Interrupt)
            {
                let mut replacement = self.queue.claim_matching_with_anchor(
                    |item| item.admission() == PromptAdmission::Interrupt,
                    |replacement, item| {
                        matches!(
                            item.admission(),
                            PromptAdmission::Steer | PromptAdmission::Interrupt
                        ) && item.mode() == replacement.mode()
                    },
                );
                replacement.sort_by_key(|(_, item)| {
                    usize::from(item.admission() == PromptAdmission::Interrupt)
                });
                replacement
            } else {
                let steered = self
                    .queue
                    .claim_one_matching(|item| item.guide_ready(next_ready, &execution_mode));
                if !steered.is_empty() {
                    steered
                } else if self
                    .queue
                    .has_matching(|item| item.guide_ready(next_ready, &execution_mode))
                {
                    self.processing.store(false, Ordering::Release);
                    return Vec::new();
                } else if next_ready {
                    self.queue.claim_compatible(
                        |item| {
                            matches!(
                                item,
                                QueueItem::Message {
                                    admission: PromptAdmission::Queue,
                                    displayed: false,
                                    ..
                                }
                            )
                        },
                        |first, item| first.mode() == item.mode(),
                    )
                } else {
                    self.queue
                        .claim_front_matching(|item| matches!(item, QueueItem::Compact { .. }))
                }
            };
            if claimed.is_empty() {
                self.processing.store(false, Ordering::Release);
                return claimed;
            }
            let claimed = claimed
                .into_iter()
                .filter(|(_, item)| item.run_id() >= min_run_id)
                .collect::<Vec<_>>();
            if claimed.is_empty() {
                continue;
            }
            if let Some((run_id, mode)) = claimed.iter().rev().find_map(|(_, item)| match item {
                QueueItem::Message { run_id, input, .. } => Some((*run_id, &input.mode)),
                QueueItem::Compact { .. } => None,
            }) {
                execution_mode.clone_from(mode);
                self.active_run_id.store(run_id, Ordering::Relaxed);
                self.active.store(true, Ordering::Release);
            }
            return claimed;
        }
    }

    fn claim_steers(&self) -> Vec<(QueueItemId, QueueItem)> {
        let _claim = self
            .claim_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.paused.load(Ordering::Acquire) || !self.dispatch_allowed() {
            return Vec::new();
        }
        if !self.active.load(Ordering::Acquire) {
            return Vec::new();
        }
        let run_id = self.active_run_id.load(Ordering::Relaxed);
        let execution_mode = self
            .execution_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.queue.claim_all_matching(|item| {
            item.admission() == PromptAdmission::Steer
                && item.run_id() == run_id
                && item.mode() == Some(&*execution_mode)
        })
    }

    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn set_active_run(&self, run_id: u64) {
        *self
            .execution_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentMode::Build;
        self.active_run_id.store(run_id, Ordering::Relaxed);
        self.active.store(true, Ordering::Release);
        self.processing.store(true, Ordering::Release);
    }

    pub(crate) fn clear_active_run(&self) {
        self.active.store(false, Ordering::Release);
        self.processing.store(false, Ordering::Release);
    }

    pub(crate) fn finish_run(&self, run_id: u64, failed: bool) -> bool {
        let _claim = self
            .claim_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let proceed = !failed || self.has_newer_interrupt(run_id);
        if !proceed {
            self.pause();
        }
        self.clear_active_run();
        proceed
    }

    pub(crate) fn has_newer_interrupt(&self, run_id: u64) -> bool {
        self.dispatch_allowed()
            && self.queue.has_matching(|item| {
                item.admission() == PromptAdmission::Interrupt && item.run_id() > run_id
            })
    }

    /// Runs `publish` under the queue lock, so a drain event can never
    /// interleave with a concurrent push.
    pub(crate) fn publish_if_empty(&self, publish: impl FnOnce()) -> bool {
        self.queue.publish_if_empty(publish)
    }

    pub(crate) async fn recv_notify(&self) -> Result<(), flume::RecvError> {
        self.queue.recv_notify().await
    }
}

impl InterruptSource for QueueReceiver {
    fn has_pending_input(&self) -> bool {
        self.queue.has_matching(|item| {
            matches!(
                item,
                QueueItem::Message {
                    displayed: false,
                    ..
                }
            )
        })
    }

    fn poll(&self) -> Option<ExtractedCommand> {
        let mut claimed = self.claim_steers();
        match claimed.len() {
            0 => None,
            1 => claimed
                .pop()
                .map(|(id, item)| item.into_extracted_command(id)),
            _ => Some(ExtractedCommand::InterruptBatch(
                claimed
                    .into_iter()
                    .filter_map(|(id, item)| match item {
                        QueueItem::Message { input, run_id, .. } => Some(QueuedInterrupt {
                            id,
                            input: *input,
                            run_id,
                        }),
                        QueueItem::Compact { .. } => None,
                    })
                    .collect(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::iter::once;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::*;
    use caudra_workspace::PlanRef;
    use test_case::test_case;

    const PADDED_DRAFT: &str = "  ab cd  ";
    const PLAN_PATH: &str = ".caudra/plans/test.md";
    const OTHER_PLAN_PATH: &str = ".caudra/plans/other.md";
    const REMOTE_PLAN: &str = "plan-1";
    const OTHER_REMOTE_PLAN: &str = "plan-2";
    const EDITED_PROMPT: &str = "edited prompt";

    fn remote_plan(reference: &str) -> AgentMode {
        AgentMode::RemotePlan(PlanRef::new(reference).unwrap())
    }

    #[test_case(PromptAdmission::Interrupt, QueueDelivery::Separate, true, &[&[2, 3], &[0], &[1]]; "replacement_priority")]
    #[test_case(PromptAdmission::Interrupt, QueueDelivery::Separate, false, &[&[2, 3], &[0], &[1]]; "replacement_while_next_held")]
    #[test_case(PromptAdmission::Interrupt, QueueDelivery::TogetherNextTurn, true, &[&[2, 3], &[0, 1]]; "replacement_preserves_together")]
    #[test_case(PromptAdmission::Interrupt, QueueDelivery::TogetherNextTurn, false, &[&[2, 3], &[0, 1]]; "held_replacement_preserves_together")]
    #[test_case(PromptAdmission::Steer, QueueDelivery::Separate, true, &[&[2], &[3], &[0], &[1]]; "guide_priority")]
    #[test_case(PromptAdmission::Steer, QueueDelivery::Separate, false, &[&[], &[2], &[3], &[0], &[1]]; "incompatible_guides_wait")]
    #[test_case(PromptAdmission::Queue, QueueDelivery::Separate, true, &[&[0], &[1], &[2], &[3]]; "separate_mixed_modes")]
    #[test_case(PromptAdmission::Queue, QueueDelivery::TogetherNextTurn, true, &[&[0, 1], &[2, 3]]; "together_mixed_modes")]
    #[test_case(PromptAdmission::Queue, QueueDelivery::TogetherNextTurn, false, &[&[], &[0, 1], &[2, 3]]; "together_waits_for_readiness")]
    fn restore_is_atomic_for_live_receiver(
        admission: PromptAdmission,
        delivery: QueueDelivery,
        ready: bool,
        expected_batches: &[&[u64]],
    ) {
        let (sender, receiver) = queue();
        let (notified_tx, notified_rx) = flume::bounded(0);
        let (receiver, mut claimed) = thread::scope(|scope| {
            let worker = scope.spawn(move || {
                smol::block_on(receiver.recv_notify()).unwrap();
                assert!(receiver.claim_gate.try_lock().is_err());
                assert_eq!(receiver.next_ready.load(Ordering::Acquire), ready);
                notified_tx.send(()).unwrap();
                let claimed = receiver.claim_idle(0);
                (receiver, claimed)
            });
            let entries = [
                AgentMode::Build,
                AgentMode::Build,
                AgentMode::Plan(PLAN_PATH.into()),
                AgentMode::Plan(PLAN_PATH.into()),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, mode)| {
                if index == 1 {
                    notified_rx.recv().unwrap();
                }
                let planning = mode.is_planning();
                let mut item = msg_with_mode(false, mode);
                item.set_run_id(index as u64);
                if planning
                    && let QueueItem::Message {
                        admission: current, ..
                    } = &mut item
                {
                    *current = admission;
                }
                item
            });
            sender.restore_pending(entries, delivery, ready);
            worker.join().unwrap()
        });

        for (index, expected) in expected_batches.iter().enumerate() {
            if index != 0 {
                receiver.hold_next_turn();
                receiver.clear_active_run();
                sender.allow_next_turn(true);
                claimed = receiver.claim_idle(0);
            }
            assert_eq!(
                claimed
                    .iter()
                    .map(|(_, item)| item.run_id())
                    .collect::<Vec<_>>(),
                *expected
            );
            if let Some((_, first)) = claimed.first() {
                assert!(claimed.iter().all(|(_, item)| item.mode() == first.mode()));
            }
        }
        assert!(sender.is_empty());
        assert_eq!(sender.delivery(), QueueDelivery::Separate);
    }

    #[test_case(false; "idle")]
    #[test_case(true; "processing")]
    fn restore_cannot_reopen_next_while_processing(processing: bool) {
        let (sender, receiver) = queue();
        if processing {
            sender.push(msg(false));
            assert_eq!(receiver.claim_idle(0).len(), 1);
        }

        sender.restore_pending([msg(false)], QueueDelivery::Separate, true);
        assert_eq!(sender.next_ready.load(Ordering::Acquire), !processing);
        receiver.clear_active_run();
        assert_eq!(receiver.claim_idle(0).is_empty(), processing);
        if processing {
            sender.allow_next_turn(true);
            assert_eq!(receiver.claim_idle(0).len(), 1);
        }
        assert!(sender.is_empty());
    }

    #[test_case(AgentMode::Build, AgentMode::Plan(PLAN_PATH.into()); "build_then_plan")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), AgentMode::Build; "plan_then_build")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), AgentMode::Plan(OTHER_PLAN_PATH.into()); "local_targets")]
    #[test_case(remote_plan(REMOTE_PLAN), remote_plan(OTHER_REMOTE_PLAN); "remote_targets")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), remote_plan(REMOTE_PLAN); "local_and_remote")]
    #[test_case(AgentMode::ReadOnly, AgentMode::Build; "read_only_and_build")]
    fn together_batches_partition_at_captured_mode(first_mode: AgentMode, second_mode: AgentMode) {
        let (sender, receiver) = queue();
        let modes = [
            first_mode.clone(),
            first_mode.clone(),
            second_mode.clone(),
            second_mode.clone(),
            first_mode.clone(),
        ];
        let ids = modes
            .iter()
            .map(|mode| sender.push(msg_with_mode(false, mode.clone())))
            .collect::<Vec<_>>();
        sender.set_delivery(QueueDelivery::TogetherNextTurn);

        for partition in [0..2, 2..4, 4..5] {
            let claimed = receiver.claim_idle(0);
            assert_eq!(
                claimed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                ids[partition.clone()]
            );
            assert!(
                claimed
                    .iter()
                    .all(|(_, item)| item.mode() == Some(&modes[partition.start]))
            );
            receiver.hold_next_turn();
            receiver.clear_active_run();
            assert!(receiver.claim_idle(0).is_empty());
            sender.allow_next_turn(true);
        }
        assert!(sender.is_empty());
        assert_eq!(sender.delivery(), QueueDelivery::Separate);
    }

    #[test_case(AgentMode::Build, AgentMode::Plan(PLAN_PATH.into()); "build_replacement")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), AgentMode::Build; "plan_replacement")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), AgentMode::Plan(OTHER_PLAN_PATH.into()); "local_targets")]
    #[test_case(remote_plan(REMOTE_PLAN), remote_plan(OTHER_REMOTE_PLAN); "remote_targets")]
    fn replacement_batches_only_compatible_guides(mode: AgentMode, other_mode: AgentMode) {
        let (sender, receiver) = queue();
        let next = sender.push(msg_with_mode(false, mode.clone()));
        let other_guide = sender.push(steer(other_mode.clone()));
        let first_guide = sender.push(steer(mode.clone()));
        let later_other_guide = sender.push(steer(other_mode.clone()));
        let second_guide = sender.push(steer(mode.clone()));
        let mut replacement = steer(mode.clone());
        if let QueueItem::Message { admission, .. } = &mut replacement {
            *admission = PromptAdmission::Interrupt;
        }
        let (replacement, _) = sender.replace(0, 1, replacement);

        let claimed = receiver.claim_idle(0);
        assert_eq!(
            claimed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [first_guide, second_guide, replacement]
        );
        assert!(claimed.iter().all(|(_, item)| item.mode() == Some(&mode)));
        receiver.hold_next_turn();
        assert!(receiver.poll().is_none());
        receiver.clear_active_run();
        assert!(!sender.has_priority_input());
        assert!(receiver.claim_idle(0).is_empty());
        sender.allow_next_turn(true);
        assert_eq!(receiver.claim_idle(0)[0].0, other_guide);
        assert_eq!(receiver.claim_idle(0)[0].0, later_other_guide);
        assert_eq!(receiver.claim_idle(0)[0].0, next);
        assert!(sender.is_empty());
    }

    #[test_case(false; "editing_incompatible_guide")]
    #[test_case(true; "editing_replacement")]
    fn incompatible_guides_cannot_outrun_replacement(edit_replacement: bool) {
        let (sender, receiver) = queue();
        let guide = sender.push(steer(AgentMode::Build));
        let mut replacement = steer(AgentMode::Plan(PLAN_PATH.into()));
        if let QueueItem::Message { admission, .. } = &mut replacement {
            *admission = PromptAdmission::Interrupt;
        }
        let (replacement, _) = sender.replace(0, 1, replacement);
        let editing = if edit_replacement { replacement } else { guide };
        assert!(sender.begin_edit(editing).is_some());
        if edit_replacement {
            assert!(receiver.claim_idle(0).is_empty());
            assert!(sender.cancel_edit(editing));
        }
        let claimed = receiver.claim_idle(0);
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].0, replacement);
        assert_eq!(sender.len(), 1);
        if !edit_replacement {
            assert!(sender.cancel_edit(editing));
        }
        assert_eq!(receiver.claim_idle(0)[0].0, guide);
    }

    #[test_case(false; "submitted_guide")]
    #[test_case(true; "moved_from_next")]
    fn mode_transition_guides_wait_for_next_ready(move_from_next: bool) {
        let (sender, receiver) = queue();
        sender.set_execution_mode(AgentMode::Build);
        receiver.set_active_run(0);
        receiver.hold_next_turn();
        let mode = AgentMode::Plan(PLAN_PATH.into());
        let plan = sender.push(if move_from_next {
            msg_with_mode(false, mode.clone())
        } else {
            steer(mode.clone())
        });
        if move_from_next {
            assert!(sender.set_admission(plan, PromptAdmission::Steer));
        }
        assert!(!sender.has_priority_input());
        assert!(receiver.poll().is_none());
        receiver.clear_active_run();
        assert!(receiver.claim_idle(0).is_empty());

        let guide = sender.push(steer(AgentMode::Build));
        assert!(sender.has_priority_input());
        assert_eq!(receiver.claim_idle(0)[0].0, guide);
        receiver.clear_active_run();
        let result = sender.push(msg(true));
        assert_eq!(receiver.claim_idle(0)[0].0, result);
        sender.allow_next_turn(true);
        receiver.clear_active_run();
        assert!(receiver.claim_idle(0).is_empty());
        sender.allow_next_turn(false);
        assert!(receiver.claim_idle(0).is_empty());
        sender.allow_next_turn(true);
        let claimed = receiver.claim_idle(0);
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].0, plan);
        assert_eq!(claimed[0].1.mode(), Some(&mode));
        assert!(sender.is_empty());
    }

    #[test_case(AgentMode::Plan(PLAN_PATH.into()); "local_plan")]
    #[test_case(remote_plan(REMOTE_PLAN); "remote_target")]
    fn restored_execution_mode_keeps_same_mode_guidance_ready(mode: AgentMode) {
        let (sender, receiver) = queue();
        sender.set_execution_mode(mode.clone());
        receiver.hold_next_turn();
        let incompatible = sender.push(steer(AgentMode::Build));
        let compatible = sender.push(steer(mode.clone()));

        assert!(sender.has_priority_input());
        let claimed = receiver.claim_idle(0);
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].0, compatible);
        assert_eq!(claimed[0].1.mode(), Some(&mode));
        receiver.clear_active_run();
        assert!(!sender.has_priority_input());
        assert!(receiver.claim_idle(0).is_empty());
        assert_eq!(sender.panel_entries()[0].id, incompatible);
    }

    #[test_case(AgentMode::Build, StoredMode::Build; "build")]
    #[test_case(AgentMode::ReadOnly, StoredMode::Build; "read_only")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), StoredMode::Plan; "local_plan")]
    #[test_case(remote_plan(REMOTE_PLAN), StoredMode::Plan; "remote_target")]
    fn edit_reorder_and_snapshot_preserve_captured_mode(mode: AgentMode, stored: StoredMode) {
        let (sender, _receiver) = queue();
        sender.push(msg(false));
        let id = sender.push(msg_with_mode(false, mode.clone()));
        assert!(sender.move_up(id));
        assert!(sender.begin_edit(id).is_some());
        assert_eq!(sender.pending_prompts()[0].mode, Some(stored));
        assert!(sender.finish_edit(
            id,
            QueuedMessage {
                text: EDITED_PROMPT.into(),
                images: Vec::new(),
                mentions: Vec::new(),
                commits: Vec::new(),
                paste_ranges: once(0..EDITED_PROMPT.len()).collect(),
            },
        ));
        assert!(sender.move_down(id));
        assert!(sender.set_admission(id, PromptAdmission::Steer));
        assert_eq!(
            sender.pending_prompts()[1],
            PendingPrompt {
                text: EDITED_PROMPT.into(),
                images: Vec::new(),
                paste_ranges: once(0..EDITED_PROMPT.len()).collect(),
                admission: PromptAdmission::Steer,
                mode: Some(stored),
            }
        );
        let item = sender.remove_id(id).unwrap();
        assert_eq!(item.mode(), Some(&mode));
    }

    #[test_case(QueueDelivery::Separate, 1; "separate")]
    #[test_case(QueueDelivery::TogetherNextTurn, 2; "together")]
    fn next_waits_for_handoff_without_blocking_guidance_or_results(
        delivery: QueueDelivery,
        count: usize,
    ) {
        let (sender, receiver) = queue();
        receiver.hold_next_turn();
        sender.set_delivery(delivery);
        let first = sender.push(msg(false));
        sender.push(msg(false));
        assert!(receiver.claim_idle(0).is_empty());
        assert!(!sender.has_priority_input());
        assert!(receiver.has_pending_input());
        let guide = sender.push(steer(AgentMode::Build));
        assert!(sender.has_priority_input());
        assert_eq!(receiver.claim_idle(0)[0].0, guide);
        receiver.clear_active_run();
        let mut result = msg(true);
        result.set_run_id(1);
        let result = sender.push(result);
        assert_eq!(receiver.claim_idle(0)[0].0, result);
        sender.allow_next_turn(true);
        receiver.clear_active_run();
        assert!(receiver.claim_idle(0).is_empty());
        sender.allow_next_turn(true);
        let claimed = receiver.claim_idle(0);
        assert_eq!(claimed.len(), count);
        assert_eq!(claimed[0].0, first);
        assert!(claimed.iter().all(|(_, item)| item.run_id() == 1));
    }

    #[test_case(false; "compact_before_next")]
    #[test_case(true; "compact_after_next")]
    fn parked_queue_preserves_compact_barriers(next_first: bool) {
        let (sender, receiver) = queue();
        receiver.hold_next_turn();
        if next_first {
            sender.push(msg(false));
        }
        sender.push(QueueItem::Compact { run_id: 0 });
        assert_eq!(receiver.claim_idle(0).is_empty(), next_first);
    }

    #[test_case(false; "idle")]
    #[test_case(true; "steer")]
    fn network_dispatch_guard_retains_queued_work_until_verified(steering: bool) {
        let (sender, receiver) = queue();
        let allowed = Arc::new(AtomicBool::new(false));
        let guard = allowed.clone();
        sender.set_dispatch_guard(Arc::new(move || guard.load(Ordering::Acquire)));
        sender.push(if steering {
            steer(AgentMode::Build)
        } else {
            msg(false)
        });
        receiver.set_active_run(0);
        if steering {
            assert!(receiver.claim_steers().is_empty());
        } else {
            assert!(receiver.claim_idle(0).is_empty());
        }
        assert_eq!(sender.len(), 1);
        allowed.store(true, Ordering::Release);
        sender.wake_dispatch();
        let claimed = if steering {
            receiver.claim_steers()
        } else {
            receiver.claim_idle(0)
        };
        assert_eq!(claimed.len(), 1);
        assert!(sender.is_empty());
    }

    fn msg(displayed: bool) -> QueueItem {
        msg_with_mode(displayed, AgentMode::Build)
    }

    fn msg_with_mode(displayed: bool, mode: AgentMode) -> QueueItem {
        QueueItem::Message {
            text: "t".into(),
            image_count: 0,
            paste_ranges: Vec::new(),
            input: Box::new(AgentInput {
                message: String::new(),
                mode,
                images: Vec::new(),
                mentions: Vec::new(),
                commits: Vec::new(),
                preamble: Vec::new(),
                thinking: Default::default(),
                fast: false,
                prompt: None,
                resume: false,
            }),
            run_id: 0,
            admission: PromptAdmission::Queue,
            displayed,
        }
    }

    fn steer(mode: AgentMode) -> QueueItem {
        let mut item = msg_with_mode(false, mode);
        if let QueueItem::Message { admission, .. } = &mut item {
            *admission = PromptAdmission::Steer;
        }
        item
    }

    #[test_case(3..5, Some(1..3) ; "shifts_by_leading_whitespace")]
    #[test_case(0..4, Some(0..2) ; "clips_leading_whitespace")]
    #[test_case(5..9, Some(3..5) ; "clips_trailing_whitespace")]
    #[test_case(0..2, None       ; "drops_range_in_leading_whitespace")]
    #[test_case(7..9, None       ; "drops_range_in_trailing_whitespace")]
    fn submission_paste_ranges_follow_the_trimmed_text(
        range: Range<usize>,
        expected: Option<Range<usize>>,
    ) {
        let message = QueuedMessage::from(Submission {
            text: PADDED_DRAFT.trim().into(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            draft: InputDraft {
                text: PADDED_DRAFT.into(),
                paste_ranges: vec![range],
            },
        });
        assert_eq!(
            message.paste_ranges,
            expected.into_iter().collect::<Vec<_>>()
        );
    }

    #[test_case(msg(false),                       true  ; "deferred_message_visible")]
    #[test_case(msg(true),                        false ; "displayed_message_hidden")]
    #[test_case(QueueItem::Compact { run_id: 0 }, true  ; "compact_visible")]
    fn panel_visibility(item: QueueItem, visible: bool) {
        let (tx, _rx) = queue();
        tx.push(item);
        assert_eq!(tx.panel_entries().len(), usize::from(visible));
    }

    #[test]
    fn nonempty_queue_does_not_publish_drain() {
        let (tx, rx) = queue();
        tx.push(msg(false));
        let called = Cell::new(false);

        rx.publish_if_empty(|| called.set(true));
        assert!(!called.get());
    }

    #[test]
    fn drain_publication_is_serialized_with_push() {
        let (tx, rx) = queue();
        let barrier = Arc::new(Barrier::new(2));
        let order = Arc::new(Mutex::new(Vec::new()));
        let worker_barrier = Arc::clone(&barrier);
        let worker_order = Arc::clone(&order);
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            tx.push(msg(false));
            worker_order.lock().unwrap().push("push");
        });

        rx.publish_if_empty(|| {
            barrier.wait();
            order.lock().unwrap().push("drain");
        });
        worker.join().unwrap();

        assert_eq!(*order.lock().unwrap(), ["drain", "push"]);
    }

    #[test]
    fn together_claim_waits_past_hidden_immediate_item_then_takes_deferred_run() {
        let (tx, rx) = queue();
        tx.push(msg(true));
        tx.push(msg(false));
        tx.push(msg(false));
        tx.set_delivery(QueueDelivery::TogetherNextTurn);

        assert_eq!(rx.claim_idle(0).len(), 1);
        assert_eq!(tx.delivery(), QueueDelivery::TogetherNextTurn);
        assert_eq!(rx.claim_idle(0).len(), 2);
        assert_eq!(tx.delivery(), QueueDelivery::Separate);
    }

    #[test]
    fn compact_is_a_hard_barrier_for_together_claim() {
        let (tx, rx) = queue();
        tx.push(QueueItem::Compact { run_id: 0 });
        tx.push(msg(false));
        tx.push(msg(false));
        tx.set_delivery(QueueDelivery::TogetherNextTurn);

        assert!(matches!(
            rx.claim_idle(0).as_slice(),
            [(_, QueueItem::Compact { .. })]
        ));
        assert_eq!(tx.delivery(), QueueDelivery::TogetherNextTurn);
        assert_eq!(rx.claim_idle(0).len(), 2);
    }

    #[test]
    fn claimed_work_stays_processing_until_cleared() {
        let (tx, rx) = queue();
        tx.push(QueueItem::Compact { run_id: 0 });

        assert!(!tx.is_processing());
        assert_eq!(rx.claim_idle(0).len(), 1);
        assert!(tx.is_processing());
        rx.clear_active_run();
        assert!(!tx.is_processing());
    }

    #[test]
    fn active_poll_claims_steers_without_consuming_queued_prompts() {
        let (tx, rx) = queue();
        tx.push(msg(false));
        let mut steer = msg(false);
        if let QueueItem::Message { admission, .. } = &mut steer {
            *admission = PromptAdmission::Steer;
        }
        tx.push(steer);
        rx.set_active_run(0);

        assert!(matches!(rx.poll(), Some(ExtractedCommand::Interrupt(..))));
        assert_eq!(tx.len(), 1);
        assert_eq!(rx.claim_idle(0).len(), 1);
    }

    #[test]
    fn active_poll_batches_all_current_steers() {
        let (tx, rx) = queue();
        for _ in 0..2 {
            let mut item = msg(false);
            if let QueueItem::Message { admission, .. } = &mut item {
                *admission = PromptAdmission::Steer;
            }
            tx.push(item);
        }
        rx.set_active_run(0);

        let Some(ExtractedCommand::InterruptBatch(inputs)) = rx.poll() else {
            panic!("expected steering batch");
        };
        assert_eq!(inputs.len(), 2);
        assert!(tx.is_empty());
    }

    #[test]
    fn active_plan_run_only_claims_plan_steers() {
        let (tx, rx) = queue();
        tx.push(msg_with_mode(
            false,
            AgentMode::Plan(PathBuf::from(PLAN_PATH)),
        ));
        assert_eq!(rx.claim_idle(0).len(), 1);
        tx.push(steer(AgentMode::Build));
        tx.push(steer(AgentMode::Plan(PathBuf::from(PLAN_PATH))));

        let Some(ExtractedCommand::Interrupt(input, ..)) = rx.poll() else {
            panic!("expected a plan steer");
        };
        assert!(matches!(input.mode, AgentMode::Plan(_)));
        assert_eq!(tx.len(), 1);
    }

    #[test_case(AgentMode::Build; "build")]
    #[test_case(AgentMode::ReadOnly; "read_only")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()); "local_plan")]
    #[test_case(remote_plan(REMOTE_PLAN); "remote_target")]
    fn active_run_only_claims_same_mode_steers(mode: AgentMode) {
        let (tx, rx) = queue();
        tx.push(msg_with_mode(false, mode.clone()));
        assert_eq!(rx.claim_idle(0).len(), 1);
        let incompatible = tx.push(steer(AgentMode::Plan(OTHER_PLAN_PATH.into())));
        let matching = tx.push(steer(mode.clone()));

        let claimed = rx.claim_steers();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].0, matching);
        assert_eq!(claimed[0].1.mode(), Some(&mode));
        assert_eq!(tx.len(), 1);
        assert_eq!(tx.panel_entries()[0].id, incompatible);
    }

    #[test]
    fn immediate_dispatch_is_claimed_before_later_priority_work() {
        let (tx, rx) = queue();
        tx.push(msg(true));
        let mut interrupt = msg(false);
        if let QueueItem::Message { admission, .. } = &mut interrupt {
            *admission = PromptAdmission::Interrupt;
        }
        tx.push(interrupt);

        let claimed = rx.claim_idle(0);

        assert!(matches!(
            claimed[0].1,
            QueueItem::Message {
                displayed: true,
                ..
            }
        ));
        assert_eq!(
            rx.claim_idle(0)[0].1.admission(),
            PromptAdmission::Interrupt
        );
    }

    #[test]
    fn active_poll_cannot_claim_steers_for_a_newer_run() {
        let (tx, rx) = queue();
        let mut steer = msg(false);
        if let QueueItem::Message {
            admission, run_id, ..
        } = &mut steer
        {
            *admission = PromptAdmission::Steer;
            *run_id = 2;
        }
        tx.push(steer);
        rx.set_active_run(1);

        assert!(rx.poll().is_none());
        assert_eq!(tx.len(), 1);
    }

    #[test]
    fn replacement_removes_unclaimed_immediate_dispatch_and_retags_pending_work() {
        let (tx, rx) = queue();
        let mut immediate = msg(true);
        immediate.set_run_id(1);
        tx.push(immediate);
        let mut pending = msg(false);
        pending.set_run_id(1);
        tx.push(pending);
        let mut old_replacement = msg(false);
        old_replacement.set_run_id(1);
        if let QueueItem::Message { admission, .. } = &mut old_replacement {
            *admission = PromptAdmission::Interrupt;
        }
        tx.push(old_replacement);
        let mut replacement = msg(false);
        replacement.set_run_id(2);
        if let QueueItem::Message { admission, .. } = &mut replacement {
            *admission = PromptAdmission::Interrupt;
        }

        tx.replace(1, 2, replacement);

        assert!(rx.has_newer_interrupt(1));
        let replacement = rx.claim_idle(0);
        assert_eq!(replacement[0].1.admission(), PromptAdmission::Interrupt);
        let pending = rx.claim_idle(0);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.run_id(), 2);
        assert!(tx.is_empty());
    }

    #[test]
    fn replacement_claim_batches_guidance_before_interrupt() {
        let (tx, rx) = queue();
        tx.push(msg(false));
        for admission in [PromptAdmission::Steer, PromptAdmission::Interrupt] {
            let mut item = msg(false);
            if let QueueItem::Message {
                admission: current, ..
            } = &mut item
            {
                *current = admission;
            }
            tx.push(item);
        }
        let mut late_steer = msg(false);
        if let QueueItem::Message { admission, .. } = &mut late_steer {
            *admission = PromptAdmission::Steer;
        }
        tx.push(late_steer);

        let replacement = rx.claim_idle(0);
        assert_eq!(replacement.len(), 3);
        assert_eq!(replacement[0].1.admission(), PromptAdmission::Steer);
        assert_eq!(replacement[1].1.admission(), PromptAdmission::Steer);
        assert_eq!(replacement[2].1.admission(), PromptAdmission::Interrupt);
        assert_eq!(rx.claim_idle(0)[0].1.admission(), PromptAdmission::Queue);
    }

    #[test]
    fn paused_queue_waits_for_ui_clear_before_accepting_new_work() {
        let (tx, rx) = queue();
        tx.push(msg(false));
        rx.pause();

        assert!(rx.claim_idle(0).is_empty());
        assert_eq!(tx.len(), 1);

        tx.clear();
        tx.push(msg(false));
        assert_eq!(rx.claim_idle(0).len(), 1);
    }
}
