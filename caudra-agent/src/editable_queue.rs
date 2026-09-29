use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptAdmission {
    #[default]
    Queue,
    Steer,
    Interrupt,
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

fn compatible_neighbor<T, K: Eq>(
    items: &VecDeque<Item<T>>,
    candidates: impl Iterator<Item = usize>,
    target_lane: &K,
    lane: &impl Fn(&T) -> Option<K>,
) -> Option<usize> {
    for candidate in candidates {
        let candidate_lane = lane(&items[candidate].value)?;
        if candidate_lane == *target_lane {
            return Some(candidate);
        }
    }
    None
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
        drop(state);
        self.notify();
    }

    pub fn remove(&self, id: QueueItemId) -> Option<T> {
        self.remove_with_delivery_guard(id, |_| true)
    }

    pub fn remove_with_delivery_guard(
        &self,
        id: QueueItemId,
        keeps_delivery: impl Fn(&T) -> bool,
    ) -> Option<T> {
        let mut state = lock(&self.state);
        let index = state.items.iter().position(|item| item.id == id)?;
        let value = state.items.remove(index).map(|item| item.value);
        if !state.items.iter().any(|item| keeps_delivery(&item.value)) {
            state.delivery = QueueDelivery::Separate;
        }
        value
    }

    pub fn update_with_delivery_guard(
        &self,
        id: QueueItemId,
        update: impl FnOnce(&mut T),
        keeps_delivery: impl Fn(&T) -> bool,
    ) -> bool {
        let mut state = lock(&self.state);
        let Some(item) = state
            .items
            .iter_mut()
            .find(|item| item.id == id && !item.editing)
        else {
            return false;
        };
        update(&mut item.value);
        if !state.items.iter().any(|item| keeps_delivery(&item.value)) {
            state.delivery = QueueDelivery::Separate;
        }
        drop(state);
        self.notify();
        true
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

    pub fn move_up_by<K: Eq>(&self, id: QueueItemId, lane: impl Fn(&T) -> Option<K>) -> bool {
        self.move_by(id, true, lane)
    }

    pub fn move_down_by<K: Eq>(&self, id: QueueItemId, lane: impl Fn(&T) -> Option<K>) -> bool {
        self.move_by(id, false, lane)
    }

    pub fn entries<R>(&self, mut map: impl FnMut(QueueItemId, &T, bool) -> R) -> Vec<R> {
        lock(&self.state)
            .items
            .iter()
            .map(|item| map(item.id, &item.value, item.editing))
            .collect()
    }

    pub fn has_matching(&self, matches: impl Fn(&T) -> bool) -> bool {
        lock(&self.state)
            .items
            .iter()
            .any(|item| matches(&item.value))
    }

    pub fn retain_mut_and_push(
        &self,
        value: T,
        mut retain: impl FnMut(&mut T) -> bool,
    ) -> QueueItemId {
        let id = QueueItemId::new();
        let mut state = lock(&self.state);
        state.items.retain_mut(|item| retain(&mut item.value));
        state.items.push_back(Item {
            id,
            value,
            editing: false,
        });
        drop(state);
        self.notify();
        id
    }

    pub fn wake(&self) {
        self.notify();
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

    fn move_by<K: Eq>(&self, id: QueueItemId, up: bool, lane: impl Fn(&T) -> Option<K>) -> bool {
        let mut state = lock(&self.state);
        let Some(index) = state.items.iter().position(|item| item.id == id) else {
            return false;
        };
        if state.items[index].editing {
            return false;
        }
        let Some(target_lane) = lane(&state.items[index].value) else {
            return false;
        };
        let neighbor = if up {
            compatible_neighbor(&state.items, (0..index).rev(), &target_lane, &lane)
        } else {
            compatible_neighbor(
                &state.items,
                index + 1..state.items.len(),
                &target_lane,
                &lane,
            )
        };
        let Some(neighbor) = neighbor else {
            return false;
        };
        if state.items[neighbor].editing {
            return false;
        }
        state.items.swap(index, neighbor);
        true
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

    pub fn claim_one_matching(&self, matches: impl Fn(&T) -> bool) -> Vec<(QueueItemId, T)> {
        let mut state = lock(&self.state);
        let Some(index) = state.items.iter().position(|item| matches(&item.value)) else {
            return Vec::new();
        };
        if state.items[index].editing {
            return Vec::new();
        }
        let claimed = state
            .items
            .remove(index)
            .map(|item| vec![(item.id, item.value)])
            .unwrap_or_default();
        if state.items.is_empty() {
            state.delivery = QueueDelivery::Separate;
        }
        claimed
    }

    pub fn claim_front_matching(&self, matches: impl Fn(&T) -> bool) -> Vec<(QueueItemId, T)> {
        let mut state = lock(&self.state);
        if !state
            .items
            .front()
            .is_some_and(|item| !item.editing && matches(&item.value))
        {
            return Vec::new();
        }
        state
            .items
            .pop_front()
            .map(|item| vec![(item.id, item.value)])
            .unwrap_or_default()
    }

    pub fn claim_all_matching(&self, matches: impl Fn(&T) -> bool) -> Vec<(QueueItemId, T)> {
        let mut state = lock(&self.state);
        let indices = state
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| matches(&item.value).then_some(index))
            .collect::<Vec<_>>();
        if indices.is_empty() || indices.iter().any(|index| state.items[*index].editing) {
            return Vec::new();
        }
        let mut claimed = indices
            .into_iter()
            .rev()
            .filter_map(|index| state.items.remove(index))
            .map(|item| (item.id, item.value))
            .collect::<Vec<_>>();
        claimed.reverse();
        if state.items.is_empty() {
            state.delivery = QueueDelivery::Separate;
        }
        claimed
    }

    pub fn publish_if_empty(&self, publish: impl FnOnce()) -> bool {
        let state = lock(&self.state);
        if state.items.is_empty() {
            publish();
            true
        } else {
            false
        }
    }

    pub fn has_matching(&self, matches: impl Fn(&T) -> bool) -> bool {
        lock(&self.state)
            .items
            .iter()
            .any(|item| matches(&item.value))
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

    pub fn move_up(&self, id: QueueItemId) -> bool {
        self.queue.move_up_by(id, |_| Some(()))
    }

    pub fn move_down(&self, id: QueueItemId) -> bool {
        self.queue.move_down_by(id, |_| Some(()))
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

    #[test]
    fn matching_claims_skip_other_items_without_reordering_them() {
        let (queue, receiver) = editable_queue();
        let queued = queue.push((PromptAdmission::Queue, "queued"));
        let first_steer = queue.push((PromptAdmission::Steer, "first steer"));
        let second_steer = queue.push((PromptAdmission::Steer, "second steer"));

        assert_eq!(
            receiver.claim_all_matching(|(admission, _)| *admission == PromptAdmission::Steer),
            [
                (first_steer, (PromptAdmission::Steer, "first steer")),
                (second_steer, (PromptAdmission::Steer, "second steer")),
            ]
        );
        assert_eq!(
            receiver.pop(),
            Some((queued, (PromptAdmission::Queue, "queued")))
        );
    }

    #[test]
    fn editing_matching_item_blocks_atomic_matching_batch() {
        let (queue, receiver) = editable_queue();
        queue.push((PromptAdmission::Steer, "first"));
        let editing = queue.push((PromptAdmission::Steer, "second"));
        queue.begin_edit(editing, |_| Some(()));

        assert!(
            receiver
                .claim_all_matching(|(admission, _)| *admission == PromptAdmission::Steer)
                .is_empty()
        );
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn matching_claim_resets_together_after_emptying_queue() {
        let (queue, receiver) = editable_queue();
        queue.push(PromptAdmission::Steer);
        queue.set_delivery(QueueDelivery::TogetherNextTurn);

        receiver.claim_all_matching(|admission| *admission == PromptAdmission::Steer);

        assert_eq!(queue.delivery(), QueueDelivery::Separate);
    }

    #[test]
    fn moves_items_in_both_directions_and_changes_claim_order() {
        let (queue, receiver) = editable_queue();
        let first = queue.push("first");
        let second = queue.push("second");
        let third = queue.push("third");

        assert!(queue.move_up_by(third, |_| Some(())));
        assert!(queue.move_down_by(first, |_| Some(())));
        queue.set_delivery(QueueDelivery::TogetherNextTurn);
        assert_eq!(
            receiver.claim(|_| true),
            [(third, "third"), (first, "first"), (second, "second")]
        );
    }

    #[test]
    fn movement_rejects_bounds_stale_ids_and_editing_items() {
        let (queue, receiver) = editable_queue();
        let first = queue.push("first");
        let second = queue.push("second");

        assert!(!queue.move_up_by(first, |_| Some(())));
        assert!(!queue.move_down_by(second, |_| Some(())));
        assert!(!queue.move_up_by(QueueItemId::new(), |_| Some(())));
        assert_eq!(queue.begin_edit(second, |_| Some(())), Some(()));
        assert!(!queue.move_up_by(second, |_| Some(())));
        assert!(!queue.move_down_by(first, |_| Some(())));
        assert!(queue.cancel_edit(second));
        assert_eq!(receiver.pop(), Some((first, "first")));
        assert_eq!(receiver.pop(), Some((second, "second")));
    }

    #[test]
    fn movement_skips_other_lanes_but_stops_at_barriers() {
        let (queue, receiver) = editable_queue();
        let first = queue.push(Some((1, "first")));
        let other = queue.push(Some((2, "other")));
        let second = queue.push(Some((1, "second")));
        queue.push(None);
        let third = queue.push(Some((1, "third")));

        assert!(queue.move_up_by(second, |item| item.map(|(lane, _)| lane)));
        assert!(!queue.move_up_by(third, |item| item.map(|(lane, _)| lane)));
        assert_eq!(
            receiver.claim_all_matching(|item| item.is_some_and(|(lane, _)| lane == 1)),
            [
                (second, Some((1, "second"))),
                (first, Some((1, "first"))),
                (third, Some((1, "third"))),
            ]
        );
        assert_eq!(receiver.pop(), Some((other, Some((2, "other")))));
    }
}
