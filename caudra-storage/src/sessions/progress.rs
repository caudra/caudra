//! Progress reporting for the storage operations that run long enough to look
//! like a hang.
//!
//! Migrating a schema rewrites every payload, and reclaiming a freelist returns
//! pages one at a time; on a large database either takes minutes. Storage has no
//! terminal of its own, so it publishes what it is doing and leaves the drawing
//! to whoever installed a sink.

use std::sync::{Arc, RwLock};

/// What a migration is doing right now.
///
/// `Backup` and `Rewrite` carry their own totals because the two phases count
/// different things, pages and rows, and neither knows the other's scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationEvent {
    /// A database needs upgrading, from its stored version up to the one this
    /// binary requires. Always precedes every other event.
    Started { from: i64, to: i64 },
    /// The pre-migration backup is copying pages.
    Backup { done: u64, total: u64 },
    /// One step of the chain began.
    Step { from: i64, to: i64 },
    /// A step is moving rows between tables.
    Rewrite {
        table: &'static str,
        done: u64,
        total: u64,
    },
    /// The pages the rewrite freed are being returned to the filesystem.
    Reclaiming { done: u64, total: u64 },
    /// The chain finished and the database is at `to`.
    Finished { from: i64, to: i64 },
}

/// What a prune is doing right now.
///
/// Only the reclaim knows how much work it has, so it owns the two measured
/// events; every other stage announces itself and is over when the next one
/// starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneEvent {
    /// A stage began that has no total to count against.
    Phase { label: &'static str },
    /// Freelist pages are being returned to the filesystem.
    Reclaiming { done: u64, total: u64 },
    /// The reclaim ended, having returned `pages`.
    Reclaimed { pages: u64 },
}

type Sink<E> = Arc<dyn Fn(E) + Send + Sync>;

/// A slot holding at most one sink for events of type `E`.
pub struct Channel<E> {
    sink: RwLock<Option<Sink<E>>>,
}

impl<E> Channel<E> {
    const fn new() -> Self {
        Self {
            sink: RwLock::new(None),
        }
    }

    /// Installs the sink events go to, replacing any earlier one. A front end
    /// calls this before it opens a database; nothing else observes storage.
    pub fn set(&self, sink: impl Fn(E) + Send + Sync + 'static) {
        if let Ok(mut slot) = self.sink.write() {
            *slot = Some(Arc::new(sink));
        }
    }

    /// Removes the installed sink, so a front end that is about to take over the
    /// terminal stops being called.
    pub fn clear(&self) {
        if let Ok(mut slot) = self.sink.write() {
            *slot = None;
        }
    }

    /// Publishes one event. Reporting is best effort: a poisoned lock or a
    /// missing sink must never fail work that is otherwise succeeding.
    pub(crate) fn report(&self, event: E) {
        let sink = self.sink.read().ok().and_then(|slot| slot.clone());
        if let Some(sink) = sink {
            sink(event);
        }
    }
}

pub static MIGRATION: Channel<MigrationEvent> = Channel::new();
pub static PRUNE: Channel<PruneEvent> = Channel::new();

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    const EVENT_REACHES_SINK: &str = "an installed sink must see what it was sent";

    #[test]
    fn a_cleared_channel_stops_receiving_events() {
        let seen: Arc<Mutex<Vec<MigrationEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        MIGRATION.set(move |event| sink.lock().unwrap().push(event));

        MIGRATION.report(MigrationEvent::Started { from: 1, to: 2 });
        MIGRATION.clear();
        MIGRATION.report(MigrationEvent::Finished { from: 1, to: 2 });

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[MigrationEvent::Started { from: 1, to: 2 }],
            "{EVENT_REACHES_SINK}"
        );
    }

    #[test]
    fn channels_of_different_events_do_not_share_a_sink() {
        let seen: Arc<Mutex<Vec<PruneEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        PRUNE.set(move |event| sink.lock().unwrap().push(event));

        MIGRATION.report(MigrationEvent::Started { from: 1, to: 2 });
        PRUNE.report(PruneEvent::Reclaimed { pages: 7 });
        PRUNE.clear();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[PruneEvent::Reclaimed { pages: 7 }],
            "{EVENT_REACHES_SINK}"
        );
    }
}
