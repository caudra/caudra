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
use std::sync::{Arc, Mutex};

use caudra_agent::{
    AgentInput, EditableQueue, EditableQueueReceiver, ExtractedCommand, ImageSource,
    InterruptSource, Mention, PromptAdmission, QueueDelivery, QueueItemId, QueuedInterrupt,
    editable_queue,
};

use crate::components::input::{InputState, Submission};
use crate::components::queue_panel::QueueEntry;
use crate::input_document::InputDraft;
use crate::theme;

const COMPACT_LABEL: &str = "/compact";

pub(crate) struct QueuedMessage {
    pub(crate) text: String,
    pub(crate) images: Vec<ImageSource>,
    pub(crate) mentions: Vec<Mention>,
    pub(crate) paste_ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingPrompt {
    pub(crate) text: String,
    pub(crate) images: Vec<ImageSource>,
    pub(crate) paste_ranges: Vec<Range<usize>>,
    pub(crate) admission: PromptAdmission,
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
    queue: EditableQueue<QueueItem>,
    paused: Arc<AtomicBool>,
    claim_gate: Arc<Mutex<()>>,
    active: Arc<AtomicBool>,
    active_run_id: Arc<AtomicU64>,
    processing: Arc<AtomicBool>,
}

pub(crate) struct QueueReceiver {
    queue: EditableQueueReceiver<QueueItem>,
    paused: Arc<AtomicBool>,
    claim_gate: Arc<Mutex<()>>,
    active: Arc<AtomicBool>,
    active_run_id: Arc<AtomicU64>,
    processing: Arc<AtomicBool>,
}

pub(crate) fn queue() -> (QueueSender, QueueReceiver) {
    let (queue, receiver) = editable_queue();
    let paused = Arc::new(AtomicBool::new(false));
    let claim_gate = Arc::new(Mutex::new(()));
    let active = Arc::new(AtomicBool::new(false));
    let active_run_id = Arc::new(AtomicU64::new(0));
    let processing = Arc::new(AtomicBool::new(false));
    (
        QueueSender {
            queue,
            paused: Arc::clone(&paused),
            claim_gate: Arc::clone(&claim_gate),
            active: Arc::clone(&active),
            active_run_id: Arc::clone(&active_run_id),
            processing: Arc::clone(&processing),
        },
        QueueReceiver {
            queue: receiver,
            paused,
            claim_gate,
            active,
            active_run_id,
            processing,
        },
    )
}

impl QueueSender {
    pub(crate) fn push(&self, entry: QueueItem) -> QueueItemId {
        self.queue.push(entry)
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
    pub(crate) fn claim_idle(&self, min_run_id: u64) -> Vec<(QueueItemId, QueueItem)> {
        let _claim = self
            .claim_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.paused.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.processing.store(true, Ordering::Release);
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
                let mut replacement = self.queue.claim_all_matching(|item| {
                    matches!(
                        item.admission(),
                        PromptAdmission::Steer | PromptAdmission::Interrupt
                    )
                });
                replacement.sort_by_key(|(_, item)| {
                    usize::from(item.admission() == PromptAdmission::Interrupt)
                });
                replacement
            } else {
                let steered = self
                    .queue
                    .claim_one_matching(|item| item.admission() == PromptAdmission::Steer);
                if !steered.is_empty() {
                    steered
                } else if self
                    .queue
                    .has_matching(|item| item.admission() == PromptAdmission::Steer)
                {
                    self.processing.store(false, Ordering::Release);
                    return Vec::new();
                } else {
                    self.queue.claim(|item| {
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
            if let Some(run_id) = claimed.iter().rev().find_map(|(_, item)| match item {
                QueueItem::Message { run_id, .. } => Some(*run_id),
                QueueItem::Compact { .. } => None,
            }) {
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
        if self.paused.load(Ordering::Acquire) {
            return Vec::new();
        }
        if !self.active.load(Ordering::Acquire) {
            return Vec::new();
        }
        let run_id = self.active_run_id.load(Ordering::Relaxed);
        self.queue.claim_all_matching(|item| {
            item.admission() == PromptAdmission::Steer && item.run_id() == run_id
        })
    }

    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn set_active_run(&self, run_id: u64) {
        self.active_run_id.store(run_id, Ordering::Relaxed);
        self.active.store(true, Ordering::Release);
    }

    pub(crate) fn clear_active_run(&self) {
        self.active.store(false, Ordering::Release);
        self.processing.store(false, Ordering::Release);
    }

    pub(crate) fn has_newer_interrupt(&self, run_id: u64) -> bool {
        self.queue.has_matching(|item| {
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
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::*;
    use test_case::test_case;

    const PADDED_DRAFT: &str = "  ab cd  ";

    fn msg(displayed: bool) -> QueueItem {
        QueueItem::Message {
            text: "t".into(),
            image_count: 0,
            paste_ranges: Vec::new(),
            input: Box::new(AgentInput {
                message: String::new(),
                mode: Default::default(),
                images: Vec::new(),
                mentions: Vec::new(),
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
