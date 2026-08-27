//! Queue of work handed from the UI to the agent loop.
//!
//! Shutdown rides on `Drop`: when the last [`QueueSender`] goes away, flume
//! closes the notify channel, so the receiver's `recv_notify` wakes with an
//! `Err` and the agent loop falls out of its main loop on its own. That way
//! nobody needs a separate "please stop" flag, and callers can't forget to
//! set it.

use std::borrow::Cow;

use maki_agent::{
    AgentInput, EditableQueue, EditableQueueReceiver, ExtractedCommand, ImageSource,
    InterruptSource, QueueDelivery, QueueItemId, QueuedInterrupt, editable_queue,
};

use crate::components::input::Submission;
use crate::components::queue_panel::QueueEntry;
use crate::theme;

const COMPACT_LABEL: &str = "/compact";

pub(crate) struct QueuedMessage {
    pub(crate) text: String,
    pub(crate) images: Vec<ImageSource>,
}

impl From<Submission> for QueuedMessage {
    fn from(sub: Submission) -> Self {
        Self {
            text: sub.text,
            images: sub.images,
        }
    }
}

pub(crate) enum QueueItem {
    Message {
        text: String,
        image_count: usize,
        input: AgentInput,
        run_id: u64,
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

impl QueueItem {
    pub(crate) fn run_id(&self) -> u64 {
        match self {
            Self::Message { run_id, .. } | Self::Compact { run_id } => *run_id,
        }
    }

    fn as_queue_entry(&self, id: QueueItemId) -> QueueEntry<'static> {
        match self {
            Self::Message { text, .. } => QueueEntry {
                id,
                text: Cow::Owned(text.clone()),
                color: theme::current().foreground,
                editable: true,
                movable: false,
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
            },
        }
    }

    fn into_extracted_command(self, id: QueueItemId) -> ExtractedCommand {
        match self {
            Self::Message { input, run_id, .. } => ExtractedCommand::Interrupt(input, run_id, id),
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
}

pub(crate) struct QueueReceiver {
    queue: EditableQueueReceiver<QueueItem>,
}

pub(crate) fn queue() -> (QueueSender, QueueReceiver) {
    let (queue, receiver) = editable_queue();
    (QueueSender { queue }, QueueReceiver { queue: receiver })
}

impl QueueSender {
    pub(crate) fn push(&self, entry: QueueItem) -> QueueItemId {
        self.queue.push(entry)
    }

    pub(crate) fn remove_id(&self, id: QueueItemId) -> Option<QueueItem> {
        self.queue.remove(id)
    }

    pub(crate) fn begin_edit(&self, id: QueueItemId) -> Option<String> {
        self.queue.begin_edit(id, |item| match item {
            QueueItem::Message { text, .. } => Some(text.clone()),
            QueueItem::Compact { .. } => None,
        })
    }

    pub(crate) fn finish_edit(&self, id: QueueItemId, text: String) -> bool {
        self.queue.finish_edit(id, |item| {
            if let QueueItem::Message {
                text: display,
                input,
                ..
            } = item
            {
                display.clone_from(&text);
                input.message = text;
            }
        })
    }

    pub(crate) fn cancel_edit(&self, id: QueueItemId) -> bool {
        self.queue.cancel_edit(id)
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn clear(&self) {
        self.queue.clear();
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

    pub(crate) fn text_messages(&self) -> Vec<String> {
        self.queue
            .entries(|_, item, _| match item {
                QueueItem::Message {
                    text, displayed, ..
                } if !displayed => Some(text.clone()),
                QueueItem::Message { .. } | QueueItem::Compact { .. } => None,
            })
            .into_iter()
            .flatten()
            .collect()
    }

    pub(crate) fn panel_len(&self) -> usize {
        self.queue
            .entries(|_, item, _| item.visible_in_panel())
            .into_iter()
            .filter(|visible| *visible)
            .count()
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        self.queue
            .entries(|id, item, _| item.visible_in_panel().then(|| item.as_queue_entry(id)))
            .into_iter()
            .flatten()
            .collect()
    }
}

impl QueueReceiver {
    pub(crate) fn claim(&self) -> Vec<(QueueItemId, QueueItem)> {
        self.queue.claim(|item| {
            matches!(
                item,
                QueueItem::Message {
                    displayed: false,
                    ..
                }
            )
        })
    }

    /// Runs `publish` under the queue lock, so a drain event can never
    /// interleave with a concurrent push.
    pub(crate) fn publish_if_empty(&self, publish: impl FnOnce()) {
        self.queue.publish_if_empty(publish);
    }

    pub(crate) async fn recv_notify(&self) -> Result<(), flume::RecvError> {
        self.queue.recv_notify().await
    }
}

impl InterruptSource for QueueReceiver {
    fn poll(&self) -> Option<ExtractedCommand> {
        let mut claimed = self.claim();
        match claimed.len() {
            0 => None,
            1 => claimed
                .pop()
                .map(|(id, item)| item.into_extracted_command(id)),
            _ => Some(ExtractedCommand::InterruptBatch(
                claimed
                    .into_iter()
                    .filter_map(|(id, item)| match item {
                        QueueItem::Message { input, run_id, .. } => {
                            Some(QueuedInterrupt { id, input, run_id })
                        }
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

    fn msg(displayed: bool) -> QueueItem {
        QueueItem::Message {
            text: "t".into(),
            image_count: 0,
            input: AgentInput {
                message: String::new(),
                mode: Default::default(),
                images: Vec::new(),
                preamble: Vec::new(),
                thinking: Default::default(),
                fast: false,
                workflow: false,
                prompt: None,
            },
            run_id: 0,
            displayed,
        }
    }

    #[test_case(msg(false),                       true  ; "deferred_message_visible")]
    #[test_case(msg(true),                        false ; "displayed_message_hidden")]
    #[test_case(QueueItem::Compact { run_id: 0 }, true  ; "compact_visible")]
    fn panel_visibility(item: QueueItem, visible: bool) {
        let (tx, _rx) = queue();
        tx.push(item);
        let expected = usize::from(visible);
        assert_eq!(tx.panel_len(), expected);
        assert_eq!(tx.panel_entries().len(), expected);
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

        assert_eq!(rx.claim().len(), 1);
        assert_eq!(tx.delivery(), QueueDelivery::TogetherNextTurn);
        assert_eq!(rx.claim().len(), 2);
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
            rx.claim().as_slice(),
            [(_, QueueItem::Compact { .. })]
        ));
        assert_eq!(tx.delivery(), QueueDelivery::TogetherNextTurn);
        assert_eq!(rx.claim().len(), 2);
    }
}
