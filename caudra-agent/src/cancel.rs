//! Cooperative cancellation with parent-to-child propagation.
//!
//! `CancelTrigger` fires on Drop, so cleanup happens even if the trigger is forgotten.
//! `cancelled()` uses a double-check around the listener to close the TOCTOU window between flag read and listener registration.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use event_listener::Event;

struct Shared {
    cancelled: AtomicBool,
    event: Event,
    /// Consulted by [`Shared::is_cancelled`] so a child never reports itself
    /// live while its parent is already cancelled. The forwarder task that
    /// wakes parked waiters is scheduled, and until it runs the child's own
    /// flag is still clear.
    parent: Option<Arc<Shared>>,
}

impl Shared {
    fn fire(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.event.notify(usize::MAX);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.is_cancelled())
    }
}

#[derive(Clone)]
pub struct CancelToken(Arc<Shared>);

pub struct CancelTrigger(Arc<Shared>);

impl CancelToken {
    pub fn new() -> (CancelTrigger, Self) {
        Self::rooted(None)
    }

    fn rooted(parent: Option<Arc<Shared>>) -> (CancelTrigger, Self) {
        let shared = Self::shared(parent);
        (CancelTrigger(Arc::clone(&shared)), Self(shared))
    }

    /// Deliberately trigger-less: [`CancelTrigger`] fires on drop, so handing
    /// one out here would cancel the token the moment it was discarded.
    pub fn none() -> Self {
        Self(Self::shared(None))
    }

    fn shared(parent: Option<Arc<Shared>>) -> Arc<Shared> {
        Arc::new(Shared {
            cancelled: AtomicBool::new(false),
            event: Event::new(),
            parent,
        })
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    pub async fn race<T>(&self, future: impl Future<Output = T>) -> Result<T, String> {
        if self.is_cancelled() {
            return Err("cancelled".into());
        }
        futures_lite::future::race(async { Ok(future.await) }, async {
            self.cancelled().await;
            Err("cancelled".into())
        })
        .await
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let listener = self.0.event.listen();
            if self.is_cancelled() {
                return;
            }
            listener.await;
        }
    }

    /// The child observes the parent's cancellation the moment it happens; the
    /// forwarder exists to wake waiters already parked on the child's event.
    pub fn child(&self) -> (CancelTrigger, Self) {
        let (child_trigger, child_token) = Self::rooted(Some(Arc::clone(&self.0)));
        let parent = self.clone();
        let child_shared = Arc::clone(&child_token.0);
        smol::spawn(async move {
            parent.cancelled().await;
            child_shared.fire();
        })
        .detach();
        (child_trigger, child_token)
    }
}

impl CancelTrigger {
    pub fn cancel(self) {
        self.0.fire();
    }
}

impl Drop for CancelTrigger {
    fn drop(&mut self) {
        self.0.fire();
    }
}

/// Names one registration inside a key's list so its owner can retire it
/// without disturbing the others registered under the same key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CancelSlot(u64);

struct Slotted {
    slot: CancelSlot,
    trigger: Option<CancelTrigger>,
}

#[derive(Default)]
struct Entry {
    registrations: Vec<Slotted>,
    cancelled: bool,
}

pub struct CancelMap<K> {
    entries: Mutex<HashMap<K, Entry>>,
    next_slot: AtomicU64,
    changed: Event,
}

impl<K: Eq + std::hash::Hash> Default for CancelMap<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + std::hash::Hash> CancelMap<K> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_slot: AtomicU64::new(0),
            changed: Event::new(),
        }
    }

    /// Registers {trigger} under {id}, alongside any already there, and
    /// returns the slot to hand back to [`retire`](Self::retire).
    pub fn insert(&self, id: K, trigger: CancelTrigger) -> CancelSlot {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let slot = CancelSlot(self.next_slot.fetch_add(1, Ordering::Relaxed));
        let entry = map.entry(id).or_default();
        let trigger = if entry.cancelled {
            drop(trigger);
            None
        } else {
            Some(trigger)
        };
        entry.registrations.push(Slotted { slot, trigger });
        slot
    }

    /// Retires one registration, dropping its trigger when it is still
    /// active and leaving its siblings alone.
    pub fn retire(&self, id: &K, slot: CancelSlot) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = map.get_mut(id) else {
            return;
        };
        entry
            .registrations
            .retain(|registration| registration.slot != slot);
        if entry.registrations.is_empty() {
            map.remove(id);
        }
        drop(map);
        self.changed.notify(usize::MAX);
    }

    /// Cancels everything under {id} and marks later siblings cancelled.
    /// The entry stays until every registered sibling retires.
    pub fn cancel_or_precancel(&self, id: K) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(id).or_default();
        entry.cancelled = true;
        for registration in &mut entry.registrations {
            drop(registration.trigger.take());
        }
        drop(map);
        self.changed.notify(usize::MAX);
    }

    pub fn remove(&self, id: &K) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.changed.notify(usize::MAX);
    }

    #[cfg(test)]
    fn has_key(&self, id: &K) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }

    pub fn cancel_all(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain();
        self.changed.notify(usize::MAX);
    }

    pub fn active_count(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|entry| {
                entry
                    .registrations
                    .iter()
                    .filter(|registration| registration.trigger.is_some())
                    .count()
            })
            .sum()
    }

    pub async fn wait_for_idle(&self) {
        loop {
            if self.active_count() == 0 {
                return;
            }
            let listener = self.changed.listen();
            if self.active_count() == 0 {
                return;
            }
            listener.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_wakes_token() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            assert!(!token.is_cancelled());
            trigger.cancel();
            token.cancelled().await;
            assert!(token.is_cancelled());
        });
    }

    #[test]
    fn child_cancelled_by_parent() {
        smol::block_on(async {
            let (parent_trigger, parent_token) = CancelToken::new();
            let (_child_trigger, child_token) = parent_token.child();
            parent_trigger.cancel();
            child_token.cancelled().await;
            assert!(child_token.is_cancelled());
        });
    }

    #[test]
    fn child_cancelled_by_own_trigger() {
        smol::block_on(async {
            let (_parent_trigger, parent_token) = CancelToken::new();
            let (child_trigger, child_token) = parent_token.child();
            child_trigger.cancel();
            child_token.cancelled().await;
            assert!(child_token.is_cancelled());
            assert!(!parent_token.is_cancelled());
        });
    }

    /// The token for work nothing can cancel. It owns no trigger, so there is
    /// nothing whose drop could quietly cancel it.
    #[test]
    fn none_is_never_cancelled() {
        let token = CancelToken::none();

        assert!(!token.is_cancelled());
        assert!(!token.clone().is_cancelled());
        assert!(!token.child().1.is_cancelled());
    }

    /// Callers such as `ensure_open` read the flag without awaiting, so a child
    /// that lagged its parent by a scheduler hop would let an already cancelled
    /// subagent issue one more model request. No `block_on` here: the forwarder
    /// task must be irrelevant to the answer.
    #[test]
    fn a_child_observes_its_parent_without_awaiting() {
        let (parent_trigger, parent_token) = CancelToken::new();
        let (_child_trigger, child_token) = parent_token.child();
        let (_grandchild_trigger, grandchild_token) = child_token.child();

        parent_trigger.cancel();

        assert!(child_token.is_cancelled());
        assert!(grandchild_token.is_cancelled());
    }

    /// Cancelling a child says nothing about the work its parent still has in
    /// flight, so the chain is read in one direction only.
    #[test]
    fn a_cancelled_child_leaves_its_parent_running() {
        let (_parent_trigger, parent_token) = CancelToken::new();
        let (child_trigger, child_token) = parent_token.child();

        child_trigger.cancel();

        assert!(child_token.is_cancelled());
        assert!(!parent_token.is_cancelled());
    }

    #[test]
    fn drop_trigger_also_cancels() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            drop(trigger);
            token.cancelled().await;
            assert!(token.is_cancelled());
        });
    }

    #[test]
    fn race_returns_value_when_not_cancelled() {
        smol::block_on(async {
            let (_trigger, token) = CancelToken::new();
            let result = token.race(async { 42 }).await;
            assert_eq!(result.unwrap(), 42);
        });
    }

    #[test]
    fn race_returns_error_when_already_cancelled() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            trigger.cancel();
            let result = token.race(std::future::pending::<()>()).await;
            assert!(result.unwrap_err().contains("cancelled"));
        });
    }

    #[test]
    fn race_interrupted_by_concurrent_cancel() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            smol::spawn(async move { trigger.cancel() }).detach();
            let result = token.race(std::future::pending::<()>()).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn cancel_map_insert_and_cancel() {
        let map = CancelMap::new();
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(!token.is_cancelled());
        map.cancel_or_precancel("t1".to_owned());
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_map_cancel_before_insert() {
        let map = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_map_remove_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.remove(&"t1".to_owned());
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(!token.is_cancelled(), "remove should clear cancellation");
    }

    #[test]
    fn cancel_map_cancel_all() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert("a".to_owned(), t1);
        map.insert("b".to_owned(), t2);
        map.cancel_all();
        assert!(tok1.is_cancelled());
        assert!(tok2.is_cancelled());
    }

    #[test]
    fn cancel_map_cancel_all_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.cancel_all();
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(
            !token.is_cancelled(),
            "cancel_all should clear cancelled entries"
        );
    }

    /// One tool call can open several subagents. They used to evict each
    /// other, so the first died the moment the second registered.
    #[test]
    fn cancel_map_keeps_siblings_under_one_key() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert("x".to_owned(), t1);
        map.insert("x".to_owned(), t2);
        assert!(!tok1.is_cancelled(), "a sibling must not evict the first");
        assert!(!tok2.is_cancelled());

        map.cancel_or_precancel("x".to_owned());
        assert!(tok1.is_cancelled(), "cancelling the key stops them all");
        assert!(tok2.is_cancelled());
    }

    #[test]
    fn cancel_map_retire_leaves_siblings_running() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        let slot1 = map.insert("x".to_owned(), t1);
        map.insert("x".to_owned(), t2);

        map.retire(&"x".to_owned(), slot1);
        assert!(tok1.is_cancelled(), "retiring drops that trigger");
        assert!(!tok2.is_cancelled(), "the sibling keeps running");

        map.cancel_or_precancel("x".to_owned());
        assert!(tok2.is_cancelled());
    }

    /// The last one out clears the key so it can be reused.
    #[test]
    fn cancel_map_retiring_the_last_registration_clears_the_key() {
        let map = CancelMap::new();
        let (t1, _tok1) = CancelToken::new();
        let slot = map.insert("x".to_owned(), t1);
        assert!(map.has_key(&"x".to_owned()));

        map.retire(&"x".to_owned(), slot);
        assert!(!map.has_key(&"x".to_owned()), "empty key must be dropped");
    }

    /// Cancelling before anything registers has to catch every session the
    /// tool call goes on to open, not just the first one through the door.
    #[test]
    fn cancel_map_precancel_catches_every_later_sibling() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("x".to_owned());

        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        let slot1 = map.insert("x".to_owned(), t1);
        let slot2 = map.insert("x".to_owned(), t2);
        assert!(tok1.is_cancelled());
        assert!(
            tok2.is_cancelled(),
            "the mark must outlive the first insert"
        );

        map.retire(&"x".to_owned(), slot1);
        assert!(map.has_key(&"x".to_owned()));
        map.retire(&"x".to_owned(), slot2);
        assert!(!map.has_key(&"x".to_owned()));
    }

    /// Pressing esc while a fan-out is running must also stop the sibling
    /// that starts a moment later.
    #[test]
    fn cancel_map_cancel_catches_a_sibling_registered_after() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let slot1 = map.insert("x".to_owned(), t1);

        map.cancel_or_precancel("x".to_owned());
        assert!(tok1.is_cancelled());

        let (t2, tok2) = CancelToken::new();
        let slot2 = map.insert("x".to_owned(), t2);
        assert!(tok2.is_cancelled(), "cancel left no mark for the sibling");

        map.retire(&"x".to_owned(), slot1);
        assert!(map.has_key(&"x".to_owned()));
        map.retire(&"x".to_owned(), slot2);
        assert!(!map.has_key(&"x".to_owned()));

        let (t3, tok3) = CancelToken::new();
        map.insert("x".to_owned(), t3);
        assert!(
            !tok3.is_cancelled(),
            "the completed call must not poison a reused tool id"
        );
    }

    #[test]
    fn cancel_map_insert_into_cancelled_returns_retirement_slot() {
        let map = CancelMap::new();
        map.cancel_or_precancel("x".to_owned());
        let (trigger, token) = CancelToken::new();
        let slot = map.insert("x".to_owned(), trigger);
        assert!(token.is_cancelled());
        map.retire(&"x".to_owned(), slot);
        assert!(!map.has_key(&"x".to_owned()));
    }
}
