//! Per-path reader/writer locks, shared by every agent in a session.
//!
//! Tools read and mutate files in several steps: a stale-read check, the write
//! itself, then recording the new mtime. Nothing about that sequence is atomic,
//! so two `batch` children targeting one file used to interleave, and the loser
//! either failed a stale check the tracker had not caught up to or silently
//! clobbered the other's edit. Dispatch takes the guards for a call's declared
//! targets and holds them across execution, which closes both windows. A
//! remote write is guarded from before its preparation instead, because the
//! host fixes the version it will replace when it prepares.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_lock::{RwLock, RwLockReadGuardArc, RwLockWriteGuardArc};

use crate::tools::file_tracker::normalize_path;

const REMOTE_SEPARATOR: &str = "/";
const CURRENT_DIRECTORY: &str = ".";
const PARENT_DIRECTORY: &str = "..";

type LockMap = HashMap<LockKey, Weak<RwLock<()>>>;

/// What a guard is keyed by. A local path is canonicalized on this machine. A
/// remote path names a file on the Workcell host, where resolving it here
/// would consult the wrong filesystem, so it is only folded lexically. The two
/// never alias, even when their text matches.
///
/// Remote sorts first because dispatch takes a remote write's keys before the
/// local targets of any call, so one acquire naming both kinds follows the
/// same global order and cannot deadlock against dispatch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockKey {
    Remote(String),
    Local(PathBuf),
}

impl LockKey {
    /// `path` joined to the remote `cwd` unless already absolute, with `.` and
    /// `..` folded, so every spelling of one host file from one base shares a
    /// key.
    pub fn remote(cwd: &str, path: &str) -> Self {
        let base = if path.starts_with(REMOTE_SEPARATOR) {
            ""
        } else {
            cwd
        };
        let mut components = Vec::new();
        for component in base
            .split(REMOTE_SEPARATOR)
            .chain(path.split(REMOTE_SEPARATOR))
        {
            match component {
                "" | CURRENT_DIRECTORY => {}
                PARENT_DIRECTORY => {
                    components.pop();
                }
                name => components.push(name),
            }
        }
        Self::Remote(format!(
            "{REMOTE_SEPARATOR}{}",
            components.join(REMOTE_SEPARATOR)
        ))
    }

    fn normalized(&self) -> Self {
        match self {
            Self::Local(path) => Self::Local(normalize_path(path)),
            Self::Remote(path) => Self::Remote(path.clone()),
        }
    }
}

/// A tool holding guards must never re-enter tool dispatch: the nested call
/// would queue behind the guards its own caller is still holding. Every current
/// implementor of `mutation_targets`, `read_targets` or `preflight_write_keys`
/// is a leaf.
#[derive(Default)]
pub struct PathLocks(Mutex<LockMap>);

enum PathGuard {
    Read(#[expect(dead_code, reason = "held to release the lock on drop")] RwLockReadGuardArc<()>),
    Write(
        #[expect(dead_code, reason = "held to release the lock on drop")] RwLockWriteGuardArc<()>,
    ),
}

/// Held for the whole of a tool's execution and released on drop, including
/// when the tool panics or its future is cancelled.
pub struct PathGuards(
    #[expect(dead_code, reason = "held to release the locks on drop")] Vec<PathGuard>,
);

impl PathLocks {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Guards are taken in sorted key order, so a multi-target call cannot
    /// deadlock against a sibling naming the same keys in the other order.
    pub async fn acquire(&self, writes: &[LockKey], reads: &[LockKey]) -> PathGuards {
        let mut wanted: Vec<(LockKey, bool)> = writes
            .iter()
            .map(|key| (key.normalized(), true))
            .chain(reads.iter().map(|key| (key.normalized(), false)))
            .collect();
        // Sorted by key, then writers first, so the dedup below keeps the
        // stronger guard. One guard per key or a call naming a file as both a
        // read and a write target would block on itself.
        wanted.sort_by(|(left, left_writes), (right, right_writes)| {
            left.cmp(right).then(right_writes.cmp(left_writes))
        });
        wanted.dedup_by(|(later, _), (earlier, _)| later == earlier);

        let mut guards = Vec::with_capacity(wanted.len());
        for (key, writes) in wanted {
            let lock = self.lock_for(key);
            guards.push(match writes {
                true => PathGuard::Write(lock.write_arc().await),
                false => PathGuard::Read(lock.read_arc().await),
            });
        }
        PathGuards(guards)
    }

    fn lock_for(&self, key: LockKey) -> Arc<RwLock<()>> {
        let mut map = self.map();
        if let Some(lock) = map.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        // Every key a session touches lands here once, so sweep the entries
        // whose last guard has gone rather than growing a map of dead weaks.
        map.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(RwLock::new(()));
        map.insert(key, Arc::downgrade(&lock));
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
    use test_case::test_case;

    use super::*;

    const CONTENDED: &str = "/tmp/caudra-path-locks/contended";
    const OTHER: &str = "/tmp/caudra-path-locks/other";

    fn paths(values: &[&str]) -> Vec<LockKey> {
        values
            .iter()
            .map(|value| LockKey::Local(PathBuf::from(value)))
            .collect()
    }

    fn remote(values: &[&str]) -> Vec<LockKey> {
        values
            .iter()
            .map(|value| LockKey::Remote((*value).to_owned()))
            .collect()
    }

    fn none() -> Vec<LockKey> {
        Vec::new()
    }

    /// Counts overlapping critical sections without timing: each task bumps the
    /// gauge on entry and reads it back, so any interleaving is recorded.
    async fn section(
        locks: &PathLocks,
        inside: &AtomicUsize,
        peak: &AtomicUsize,
        targets: Vec<LockKey>,
    ) {
        let guards = locks.acquire(&targets, &none()).await;
        let depth = inside.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(depth, Ordering::SeqCst);
        future::yield_now().await;
        inside.fetch_sub(1, Ordering::SeqCst);
        drop(guards);
    }

    async fn peak_overlap(locks: &PathLocks, first: Vec<LockKey>, second: Vec<LockKey>) -> usize {
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
    fn a_local_path_and_a_remote_key_with_the_same_text_never_contend() {
        let locks = PathLocks::default();
        let peak = future::block_on(peak_overlap(
            &locks,
            paths(&[CONTENDED]),
            remote(&[CONTENDED]),
        ));
        assert_eq!(peak, 2);
    }

    /// Dispatch takes a remote write's keys in one acquire and local targets
    /// in a later one. A call naming both kinds at once has to take them in
    /// that order too, or each could hold what the other waits for.
    #[test]
    fn one_acquire_naming_both_kinds_follows_the_dispatch_order() {
        let locks = PathLocks::default();
        let inside = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let dispatched = async {
            let remote_guards = locks.acquire(&remote(&[CONTENDED]), &none()).await;
            future::yield_now().await;
            let local_guards = locks.acquire(&paths(&[CONTENDED]), &none()).await;
            let depth = inside.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(depth, Ordering::SeqCst);
            future::yield_now().await;
            inside.fetch_sub(1, Ordering::SeqCst);
            drop((local_guards, remote_guards));
        };
        let mixed = section(
            &locks,
            &inside,
            &peak,
            [paths(&[CONTENDED]), remote(&[CONTENDED])].concat(),
        );
        future::block_on(future::zip(dispatched, mixed));
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test_case("relative/file"; "relative")]
    #[test_case("/workspace/project/relative/file"; "absolute")]
    #[test_case("./nested/../relative//file"; "folded")]
    #[test_case("../project/relative/file"; "through_the_parent")]
    fn every_spelling_of_one_remote_file_shares_a_key(spelling: &str) {
        const REMOTE_CWD: &str = "/workspace/project";
        const REMOTE_FILE: &str = "/workspace/project/relative/file";
        assert_eq!(
            LockKey::remote(REMOTE_CWD, spelling),
            LockKey::Remote(REMOTE_FILE.to_owned())
        );
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
