//! Per-path reader/writer locks, shared by every agent in a session.
//!
//! Tools read and mutate files in several steps: a stale-read check, the write
//! itself, then recording the new mtime. Nothing about that sequence is atomic,
//! so two `batch` children targeting one file used to interleave, and the loser
//! either failed a stale check the tracker had not caught up to or silently
//! clobbered the other's edit. Dispatch takes the guards for a call's declared
//! targets and holds them across execution, which closes both windows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_lock::{RwLock, RwLockReadGuardArc, RwLockWriteGuardArc};

use crate::tools::file_tracker::normalize_path;

type LockMap = HashMap<PathBuf, Weak<RwLock<()>>>;

/// A tool holding guards must never re-enter tool dispatch: the nested call
/// would queue behind the guards its own caller is still holding. Every current
/// implementor of `mutation_targets` or `read_targets` is a leaf.
#[derive(Default)]
pub struct PathLocks(Mutex<LockMap>);

enum PathGuard {
    Read(#[expect(dead_code, reason = "held to release the lock on drop")] RwLockReadGuardArc<()>),
    Write(#[expect(dead_code, reason = "held to release the lock on drop")] RwLockWriteGuardArc<()>),
}

/// Held for the whole of a tool's execution and released on drop, including
/// when the tool panics or its future is cancelled.
pub struct PathGuards(#[expect(dead_code, reason = "held to release the locks on drop")] Vec<PathGuard>);

impl PathLocks {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Guards are taken in sorted path order, so a multi-target call cannot
    /// deadlock against a sibling naming the same paths in the other order.
    pub async fn acquire(&self, writes: &[PathBuf], reads: &[PathBuf]) -> PathGuards {
        let mut wanted: Vec<(PathBuf, bool)> = writes
            .iter()
            .map(|path| (normalize_path(path), true))
            .chain(reads.iter().map(|path| (normalize_path(path), false)))
            .collect();
        // Sorted by path, then writers first, so the dedup below keeps the
        // stronger guard. One guard per path or a call naming a file as both a
        // read and a write target would block on itself.
        wanted.sort_by(|(left, left_writes), (right, right_writes)| {
            left.cmp(right).then(right_writes.cmp(left_writes))
        });
        wanted.dedup_by(|(later, _), (earlier, _)| later == earlier);

        let mut guards = Vec::with_capacity(wanted.len());
        for (path, writes) in wanted {
            let lock = self.lock_for(path);
            guards.push(match writes {
                true => PathGuard::Write(lock.write_arc().await),
                false => PathGuard::Read(lock.read_arc().await),
            });
        }
        PathGuards(guards)
    }

    fn lock_for(&self, path: PathBuf) -> Arc<RwLock<()>> {
        let mut map = self.map();
        if let Some(lock) = map.get(&path).and_then(Weak::upgrade) {
            return lock;
        }
        // Every path a session touches lands here once, so sweep the entries
        // whose last guard has gone rather than growing a map of dead weaks.
        map.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(RwLock::new(()));
        map.insert(path, Arc::downgrade(&lock));
        lock
    }

    fn map(&self) -> MutexGuard<'_, LockMap> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.map().len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_lite::future;

    use super::*;

    const CONTENDED: &str = "/tmp/caudra-path-locks/contended";
    const OTHER: &str = "/tmp/caudra-path-locks/other";

    fn paths(values: &[&str]) -> Vec<PathBuf> {
        values.iter().map(PathBuf::from).collect()
    }

    fn none() -> Vec<PathBuf> {
        Vec::new()
    }

    /// Counts overlapping critical sections without timing: each task bumps the
    /// gauge on entry and reads it back, so any interleaving is recorded.
    async fn section(
        locks: &PathLocks,
        inside: &AtomicUsize,
        peak: &AtomicUsize,
        targets: Vec<PathBuf>,
    ) {
        let guards = locks.acquire(&targets, &none()).await;
        let depth = inside.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(depth, Ordering::SeqCst);
        future::yield_now().await;
        inside.fetch_sub(1, Ordering::SeqCst);
        drop(guards);
    }

    async fn peak_overlap(locks: &PathLocks, first: Vec<PathBuf>, second: Vec<PathBuf>) -> usize {
        let inside = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        future::zip(
            section(locks, &inside, &peak, first),
            section(locks, &inside, &peak, second),
        )
        .await;
        peak.load(Ordering::SeqCst)
    }

    #[test]
    fn writers_on_one_path_never_overlap() {
        let locks = PathLocks::default();
        let peak = future::block_on(peak_overlap(
            &locks,
            paths(&[CONTENDED]),
            paths(&[CONTENDED]),
        ));
        assert_eq!(peak, 1);
    }

    #[test]
    fn writers_on_distinct_paths_overlap() {
        let locks = PathLocks::default();
        let peak = future::block_on(peak_overlap(&locks, paths(&[CONTENDED]), paths(&[OTHER])));
        assert_eq!(peak, 2);
    }

    #[test]
    fn readers_share_one_path() {
        let locks = PathLocks::default();
        future::block_on(async {
            let first = locks.acquire(&none(), &paths(&[CONTENDED])).await;
            let second = locks.acquire(&none(), &paths(&[CONTENDED])).await;
            drop((first, second));
        });
    }

    #[test]
    fn a_writer_excludes_a_reader() {
        let locks = PathLocks::default();
        let reader_ran = AtomicUsize::new(0);
        future::block_on(async {
            let write = locks.acquire(&paths(&[CONTENDED]), &none()).await;
            let reader = async {
                let guards = locks.acquire(&none(), &paths(&[CONTENDED])).await;
                reader_ran.fetch_add(1, Ordering::SeqCst);
                drop(guards);
            };
            let releaser = async {
                future::yield_now().await;
                assert_eq!(reader_ran.load(Ordering::SeqCst), 0);
                drop(write);
            };
            future::zip(reader, releaser).await;
        });
        assert_eq!(reader_ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn opposed_multi_target_acquires_do_not_deadlock() {
        let locks = PathLocks::default();
        let peak = future::block_on(peak_overlap(
            &locks,
            paths(&[CONTENDED, OTHER]),
            paths(&[OTHER, CONTENDED]),
        ));
        assert_eq!(peak, 1);
    }

    #[test]
    fn a_path_named_as_both_read_and_write_takes_one_guard() {
        let locks = PathLocks::default();
        future::block_on(async {
            drop(
                locks
                    .acquire(&paths(&[CONTENDED]), &paths(&[CONTENDED]))
                    .await,
            );
        });
        assert_eq!(locks.tracked(), 1);
    }

    #[test]
    fn released_locks_are_swept_on_the_next_insert() {
        let locks = PathLocks::default();
        future::block_on(async {
            drop(locks.acquire(&paths(&[CONTENDED]), &none()).await);
            assert_eq!(locks.tracked(), 1);
            drop(locks.acquire(&paths(&[OTHER]), &none()).await);
        });
        assert_eq!(locks.tracked(), 1);
    }
}
