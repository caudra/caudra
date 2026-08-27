use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;

use crate::{AgentInput, ExtractedCommand, InterruptSource};

static NEXT_QUEUE_ITEM_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct QueueItemId(u64);

impl QueueItemId {
    pub fn new() -> Self {
        Self(NEXT_QUEUE_ITEM_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for QueueItemId {
    fn default() -> Self {
        Self::new()
    }
}

struct Item<T> {
    id: QueueItemId,
    value: T,
    editing: bool,
}

struct State<T> {
    items: VecDeque<Item<T>>,
    delivery: QueueDelivery,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum QueueDelivery {
    #[default]
    Separate,
    TogetherNextTurn,
}

pub struct EditableQueue<T> {
    state: Arc<Mutex<State<T>>>,
    notify_tx: flume::Sender<()>,
}

impl<T> Clone for EditableQueue<T> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            notify_tx: self.notify_tx.clone(),
        }
    }
}

impl<T> fmt::Debug for EditableQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditableQueue")
            .field("len", &self.len())
            .finish()
    }
}

pub struct EditableQueueReceiver<T> {
    state: Arc<Mutex<State<T>>>,
    notify_rx: flume::Receiver<()>,
}

pub fn editable_queue<T>() -> (EditableQueue<T>, EditableQueueReceiver<T>) {
    let (notify_tx, notify_rx) = flume::bounded(1);
    let state = Arc::new(Mutex::new(State {
        items: VecDeque::new(),
        delivery: QueueDelivery::Separate,
    }));
    (
        EditableQueue {
            state: Arc::clone(&state),
            notify_tx,
        },
        EditableQueueReceiver { state, notify_rx },
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<T> EditableQueue<T> {
    pub fn push(&self, value: T) -> QueueItemId {
        let id = QueueItemId::new();
        lock(&self.state).items.push_back(Item {
            id,
            value,
            editing: false,
        });
        self.notify();
        id
    }

    pub fn len(&self) -> usize {
        lock(&self.state).items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        let mut state = lock(&self.state);
        state.items.clear();
        state.delivery = QueueDelivery::Separate;
    }

    pub fn remove(&self, id: QueueItemId) -> Option<T> {
        let mut state = lock(&self.state);
        let index = state.items.iter().position(|item| item.id == id)?;
        let value = state.items.remove(index).map(|item| item.value);
        if state.items.is_empty() {
            state.delivery = QueueDelivery::Separate;
        }
        value
    }

    pub fn begin_edit<R>(
        &self,
        id: QueueItemId,
        snapshot: impl FnOnce(&T) -> Option<R>,
    ) -> Option<R> {
        let mut state = lock(&self.state);
        let item = state.items.iter_mut().find(|item| item.id == id)?;
        if item.editing {
            return None;
        }
        let snapshot = snapshot(&item.value)?;
        item.editing = true;
        Some(snapshot)
    }

    pub fn finish_edit(&self, id: QueueItemId, update: impl FnOnce(&mut T)) -> bool {
        let mut state = lock(&self.state);
        let Some(item) = state
            .items
            .iter_mut()
            .find(|item| item.id == id && item.editing)
        else {
            return false;
        };
        update(&mut item.value);
        item.editing = false;
        drop(state);
        self.notify();
        true
    }

    pub fn cancel_edit(&self, id: QueueItemId) -> bool {
        self.finish_edit(id, |_| {})
    }

    pub fn entries<R>(&self, mut map: impl FnMut(QueueItemId, &T, bool) -> R) -> Vec<R> {
        lock(&self.state)
            .items
            .iter()
            .map(|item| map(item.id, &item.value, item.editing))
            .collect()
    }

    pub fn drain(&self) -> Vec<(QueueItemId, T)> {
        let mut state = lock(&self.state);
        state.delivery = QueueDelivery::Separate;
        state
            .items
            .drain(..)
            .map(|item| (item.id, item.value))
            .collect()
    }

    pub fn delivery(&self) -> QueueDelivery {
        lock(&self.state).delivery
    }

    pub fn set_delivery(&self, delivery: QueueDelivery) {
        lock(&self.state).delivery = delivery;
    }

    pub fn toggle_delivery(&self) -> QueueDelivery {
        let mut state = lock(&self.state);
        state.delivery = match state.delivery {
            QueueDelivery::Separate => QueueDelivery::TogetherNextTurn,
            QueueDelivery::TogetherNextTurn => QueueDelivery::Separate,
        };
        state.delivery
    }

    fn notify(&self) {
        let _ = self.notify_tx.try_send(());
    }
}

impl<T> EditableQueueReceiver<T> {
    pub fn pop(&self) -> Option<(QueueItemId, T)> {
        self.claim(|_| false).pop()
    }

    pub fn claim(&self, eligible: impl Fn(&T) -> bool) -> Vec<(QueueItemId, T)> {
        let mut state = lock(&self.state);
        let Some(front) = state.items.front() else {
            return Vec::new();
        };
        if front.editing {
            return Vec::new();
        }
        if state.delivery == QueueDelivery::Separate || !eligible(&front.value) {
            return state
                .items
                .pop_front()
                .map(|item| vec![(item.id, item.value)])
                .unwrap_or_default();
        }

        let count = state
            .items
            .iter()
            .take_while(|item| eligible(&item.value))
            .count();
        if state.items.iter().take(count).any(|item| item.editing) {
            return Vec::new();
        }
        state.delivery = QueueDelivery::Separate;
        state
            .items
            .drain(..count)
            .map(|item| (item.id, item.value))
            .collect()
    }

    pub fn publish_if_empty(&self, publish: impl FnOnce()) {
        if lock(&self.state).items.is_empty() {
            publish();
        }
    }

    pub async fn recv_notify(&self) -> Result<(), flume::RecvError> {
        self.notify_rx.recv_async().await
    }
}

#[derive(Debug, Clone)]
pub struct SteeringQueue {
    queue: EditableQueue<AgentInput>,
}

pub struct SteeringQueueReceiver {
    queue: EditableQueueReceiver<AgentInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringQueueEntry {
    pub id: QueueItemId,
    pub text: String,
    pub editing: bool,
}

pub fn steering_queue() -> (SteeringQueue, SteeringQueueReceiver) {
    let (queue, receiver) = editable_queue();
    (
        SteeringQueue { queue },
        SteeringQueueReceiver { queue: receiver },
    )
}

impl SteeringQueue {
    pub fn push(&self, input: AgentInput) -> QueueItemId {
        self.queue.push(input)
    }

    pub fn entries(&self) -> Vec<SteeringQueueEntry> {
        self.queue.entries(|id, input, editing| SteeringQueueEntry {
            id,
            text: input.message.clone(),
            editing,
        })
    }

    pub fn remove(&self, id: QueueItemId) -> Option<AgentInput> {
        self.queue.remove(id)
    }

    pub fn begin_edit(&self, id: QueueItemId) -> Option<String> {
        self.queue
            .begin_edit(id, |input| Some(input.message.clone()))
    }

    pub fn finish_edit(&self, id: QueueItemId, text: String) -> bool {
        self.queue.finish_edit(id, |input| input.message = text)
    }

    pub fn cancel_edit(&self, id: QueueItemId) -> bool {
        self.queue.cancel_edit(id)
    }

    pub fn drain(&self) -> Vec<(QueueItemId, AgentInput)> {
        self.queue.drain()
    }

    pub fn delivery(&self) -> QueueDelivery {
        self.queue.delivery()
    }

    pub fn set_delivery(&self, delivery: QueueDelivery) {
        self.queue.set_delivery(delivery);
    }

    pub fn toggle_delivery(&self) -> QueueDelivery {
        self.queue.toggle_delivery()
    }
}

impl InterruptSource for SteeringQueueReceiver {
    fn poll(&self) -> Option<ExtractedCommand> {
        let mut inputs = self
            .queue
            .claim(|_| true)
            .into_iter()
            .map(|(id, input)| crate::QueuedInterrupt {
                id,
                input,
                run_id: 0,
            })
            .collect::<Vec<_>>();
        match inputs.len() {
            0 => None,
            1 => inputs
                .pop()
                .map(|queued| ExtractedCommand::Interrupt(queued.input, queued.run_id, queued.id)),
            _ => Some(ExtractedCommand::InterruptBatch(inputs)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_front_blocks_consumption_until_released() {
        let (queue, receiver) = editable_queue();
        let first = queue.push("first");
        queue.push("second");

        assert_eq!(
            queue.begin_edit(first, |text| Some(text.to_string())),
            Some("first".to_string())
        );
        assert!(receiver.pop().is_none());
        assert!(queue.finish_edit(first, |text| *text = "edited"));
        assert_eq!(receiver.pop(), Some((first, "edited")));
    }

    #[test]
    fn stale_id_does_not_remove_next_item() {
        let (queue, receiver) = editable_queue();
        let first = queue.push("first");
        let second = queue.push("second");

        assert_eq!(receiver.pop(), Some((first, "first")));
        assert_eq!(queue.remove(first), None);
        assert_eq!(receiver.pop(), Some((second, "second")));
    }

    #[test]
    fn dropping_last_sender_disconnects_receiver() {
        let (queue, receiver) = editable_queue::<()>();
        drop(queue);

        assert!(matches!(
            receiver.notify_rx.try_recv(),
            Err(flume::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn together_claim_includes_messages_added_after_toggle_and_resets() {
        let (queue, receiver) = editable_queue();
        let first = queue.push("first");
        queue.set_delivery(QueueDelivery::TogetherNextTurn);
        let second = queue.push("second");

        assert_eq!(
            receiver.claim(|_| true),
            [(first, "first"), (second, "second")]
        );
        assert_eq!(queue.delivery(), QueueDelivery::Separate);
    }

    #[test]
    fn editing_any_batch_member_blocks_partial_claim() {
        let (queue, receiver) = editable_queue();
        queue.push("first");
        let second = queue.push("second");
        queue.set_delivery(QueueDelivery::TogetherNextTurn);
        queue.begin_edit(second, |text| Some(text.to_string()));

        assert!(receiver.claim(|_| true).is_empty());
        assert!(queue.cancel_edit(second));
        assert_eq!(receiver.claim(|_| true).len(), 2);
    }

    #[test]
    fn ineligible_front_item_is_a_barrier_without_resetting_delivery() {
        let (queue, receiver) = editable_queue();
        let barrier = queue.push(0);
        let first = queue.push(1);
        let second = queue.push(2);
        queue.set_delivery(QueueDelivery::TogetherNextTurn);

        assert_eq!(receiver.claim(|value| *value > 0), [(barrier, 0)]);
        assert_eq!(queue.delivery(), QueueDelivery::TogetherNextTurn);
        assert_eq!(
            receiver.claim(|value| *value > 0),
            [(first, 1), (second, 2)]
        );
    }

    #[test]
    fn deleting_last_item_resets_together_delivery() {
        let (queue, _receiver) = editable_queue();
        let id = queue.push("only");
        queue.set_delivery(QueueDelivery::TogetherNextTurn);

        assert_eq!(queue.remove(id), Some("only"));
        assert_eq!(queue.delivery(), QueueDelivery::Separate);
    }
}
