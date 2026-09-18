//! Progress reporting for schema migrations.
//!
//! A migration can rewrite every payload in the database, which takes long
//! enough that a silent startup reads as a hang. Storage has no terminal of its
//! own, so it publishes what it is doing and leaves the drawing to whoever
//! installed a reporter.

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
    /// The chain finished and the database is at `to`.
    Finished { from: i64, to: i64 },
}

type Reporter = Arc<dyn Fn(MigrationEvent) + Send + Sync>;

static REPORTER: RwLock<Option<Reporter>> = RwLock::new(None);

/// Installs the sink migrations report to, replacing any earlier one. A front
/// end calls this before it opens a database; nothing else observes migrations.
pub fn set_reporter(reporter: impl Fn(MigrationEvent) + Send + Sync + 'static) {
    if let Ok(mut slot) = REPORTER.write() {
        *slot = Some(Arc::new(reporter));
    }
}

/// Removes the installed reporter, so a front end that is about to take over the
/// terminal stops being called.
pub fn clear_reporter() {
    if let Ok(mut slot) = REPORTER.write() {
        *slot = None;
    }
}

/// Publishes one event. Reporting is best effort: a poisoned lock or a missing
/// reporter must never fail a migration that is otherwise succeeding.
pub(crate) fn report(event: MigrationEvent) {
    let reporter = REPORTER.read().ok().and_then(|slot| slot.clone());
    if let Some(reporter) = reporter {
        reporter(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    const EVENT_REACHES_REPORTER: &str = "an installed reporter must see what it was sent";

    #[test]
    fn a_cleared_reporter_stops_receiving_events() {
        let seen: Arc<Mutex<Vec<MigrationEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        set_reporter(move |event| sink.lock().unwrap().push(event));

        report(MigrationEvent::Started { from: 1, to: 2 });
        clear_reporter();
        report(MigrationEvent::Finished { from: 1, to: 2 });

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[MigrationEvent::Started { from: 1, to: 2 }],
            "{EVENT_REACHES_REPORTER}"
        );
    }
}
