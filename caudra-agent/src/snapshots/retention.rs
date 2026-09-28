//! Keeping a workspace store within its cap. Every session of a workspace
//! shares its objects, so eviction weighs the checkpoints of all of them, and
//! collection keeps whatever any of them still names.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use caudra_storage::id::CaudraId;
use caudra_storage::{StateDir, lock_session_artifacts};
use tracing::warn;
use workcell::snapshot_store::ObjectStore;

use super::restore::restore_objects;
use super::storage::{
    START_POINTER, checkpoint_pointers, open_repository, read_pointer, remove_if_present,
    subdirectories,
};
use super::{
    SESSION_SNAPSHOTS_DIR, SnapshotError, SnapshotKey, SnapshotStore, WORKSPACE_SNAPSHOTS_DIR,
};

/// A running estimate of the store's size, so a capture need not measure the
/// whole store to learn it is still under the cap. Exact after a collection;
/// each capture then adds the uncompressed size of the blobs it wrote, which
/// only ever overstates what compression leaves on disk.
const USAGE_NAME: &str = "usage";

impl SnapshotStore {
    /// Once the running usage passes the cap, collects garbage and then evicts
    /// the oldest checkpoints across every session of the workspace, in
    /// doubling batches, until the store fits. Each session keeps its start and
    /// its newest checkpoint, and this one keeps the `written` capture, so what
    /// is protected can still exceed the cap.
    pub(super) fn enforce_cap(
        &self,
        repository: &ObjectStore,
        new_bytes: u64,
        written: SnapshotKey,
    ) -> Result<(), SnapshotError> {
        let recorded = read_usage(repository);
        let usage = match recorded {
            Some(recorded) => recorded.saturating_add(new_bytes),
            None => repository.usage()?.bytes,
        };
        if usage <= self.cap_bytes {
            return match recorded {
                Some(recorded) if recorded == usage => Ok(()),
                _ => write_usage(repository, usage),
            };
        }
        let sessions = self.sessions_dir();
        let key = self.workspace_key();
        let mut usage = collect(repository, sessions, key)?;
        let evictable = evictable_checkpoints(sessions, key, &self.pointer_path(written));
        let mut remaining = evictable.as_slice();
        let mut batch = 1;
        while usage > self.cap_bytes && !remaining.is_empty() {
            let (evicted, rest) = remaining.split_at(batch.min(remaining.len()));
            evicted
                .iter()
                .try_for_each(|pointer| remove_if_present(pointer))?;
            remaining = rest;
            batch *= 2;
            usage = collect(repository, sessions, key)?;
        }
        Ok(())
    }
}

/// Collects the garbage of every workspace store in `state`, so the objects
/// only trimmed or forgotten sessions used are freed now rather than at the
/// next capture over the cap. Answers the bytes freed. A store that cannot be
/// collected is logged and left for its next capture over the cap: it must not
/// keep every other workspace's garbage on disk.
pub fn collect_garbage(state: &StateDir) -> Result<u64, SnapshotError> {
    let _lock = lock_session_artifacts(state).map_err(io::Error::other)?;
    let sessions = state.path().join(SESSION_SNAPSHOTS_DIR);
    let mut freed = 0;
    for (key, dir) in subdirectories(&state.path().join(WORKSPACE_SNAPSHOTS_DIR))? {
        let collected = open_repository(&dir).and_then(|repository| {
            let before = repository.usage()?.bytes;
            Ok(before.saturating_sub(collect(&repository, &sessions, &key)?))
        });
        match collected {
            Ok(bytes) => freed += bytes,
            Err(error) => warn!(workspace = %key, %error, "could not collect a snapshot store"),
        }
    }
    Ok(freed)
}

/// Deletes every object that no session's pointers, restore journal or
/// unrevert record names, nor the stat cache, and answers the exact usage
/// left. Any root it cannot read fails the collection rather than protecting
/// nothing.
fn collect(repository: &ObjectStore, sessions: &Path, key: &str) -> Result<u64, SnapshotError> {
    let mut snapshots = Vec::new();
    let mut objects = Vec::new();
    for (_, session) in subdirectories(sessions)? {
        let dir = session.join(key);
        snapshots.extend(read_pointer(&dir.join(START_POINTER))?);
        for (_, pointer) in checkpoint_pointers(&dir)? {
            snapshots.extend(read_pointer(&pointer)?);
        }
        objects.extend(restore_objects(&dir)?);
    }
    let plan = repository.garbage_keeping(&snapshots, &objects)?;
    repository.collect(&plan)?;
    let usage = repository.usage()?.bytes;
    write_usage(repository, usage)?;
    Ok(usage)
}

/// Every checkpoint pointer of the workspace that eviction may remove, oldest
/// first: all but each session's newest, and never `protected`. A directory
/// it cannot list only evicts less.
fn evictable_checkpoints(sessions: &Path, key: &str, protected: &Path) -> Vec<PathBuf> {
    let mut evictable: Vec<(CaudraId, PathBuf)> = Vec::new();
    for (_, session) in subdirectories(sessions).unwrap_or_default() {
        let mut checkpoints = checkpoint_pointers(&session.join(key)).unwrap_or_default();
        checkpoints.pop();
        evictable.extend(
            checkpoints
                .into_iter()
                .filter(|(_, pointer)| pointer != protected),
        );
    }
    evictable.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
    evictable.into_iter().map(|(_, pointer)| pointer).collect()
}

fn read_usage(repository: &ObjectStore) -> Option<u64> {
    fs::read_to_string(repository.dir().join(USAGE_NAME))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Deferred rather than durable: a usage lost to a crash is measured again.
fn write_usage(repository: &ObjectStore, usage: u64) -> Result<(), SnapshotError> {
    caudra_storage::atomic_write_deferred(
        &repository.dir().join(USAGE_NAME),
        usage.to_string().as_bytes(),
    )
    .map_err(|error| io::Error::other(error).into())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use workcell::snapshot_store::blob_id;

    use super::*;
    use crate::snapshots::storage::{OBJECTS_DIR, checkpoints_dir};
    use crate::snapshots::tests::{
        ALPHA, BETA, KEY, OTHER_KEY, OTHER_SESSION, SESSION, id, paths, read, session_store, setup,
        state_root, store_in, write,
    };

    const EVICTED_MSG: &str = "the cap evicts old checkpoints and frees what only they named";
    const KEPT_MSG: &str = "the cap never evicts a session's start or newest checkpoint";
    const UNPRESSED_MSG: &str = "a store under its cap collects nothing";
    const PRESSED_MSG: &str = "a store over its cap collects what no snapshot names";
    const UNREVERT_KEPT_MSG: &str =
        "collection keeps what an unrevert still needs, though no snapshot names it";
    const SESSIONS_MSG: &str = "eviction weighs every session of the workspace, oldest first";
    const UNIQUE_MSG: &str =
        "collection frees what only a deleted session named, and nothing another still names";
    const UNLISTABLE_MSG: &str = "collection frees nothing a directory it cannot list may name";
    const RETENTION_FAILURE_MSG: &str = "a capture stands though its retention cannot run";
    const MOVED_EXTENSION: &str = "moved";
    const GAMMA: &str = "gamma";
    /// Below the size of any store, so every capture is over it.
    const TIGHT_CAP: u64 = 1;

    fn holds(store: &SnapshotStore, content: &str) -> bool {
        store
            .open_repository()
            .unwrap()
            .contains(&blob_id(content.as_bytes()).unwrap())
    }

    #[test]
    fn the_cap_evicts_old_checkpoints_but_keeps_the_start_and_the_newest() {
        let (temp, root) = setup();
        write(&root, "start.txt", "start");
        let store = store_in(&temp).with_cap(TIGHT_CAP);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "old.txt", "old");
        let old = id(1);
        store.snapshot(&root, old).unwrap();
        fs::remove_file(root.join("old.txt")).unwrap();
        write(&root, "new.txt", "new");
        let new = id(2);
        store.snapshot(&root, new).unwrap();

        assert!(!store.has_checkpoint(old), "{EVICTED_MSG}");
        assert!(!holds(&store, "old"), "{EVICTED_MSG}");
        assert!(store.has_session_start(), "{KEPT_MSG}");
        assert!(store.has_checkpoint(new), "{KEPT_MSG}");
        assert_eq!(
            paths(&store, SnapshotKey::Checkpoint(new)),
            ["new.txt", "start.txt"],
            "{KEPT_MSG}"
        );
    }

    /// The cap is a target, not a limit: what a session needs to restore at all
    /// stays even when it alone is over.
    #[test]
    fn the_start_and_the_newest_checkpoint_survive_a_cap_they_exceed() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp).with_cap(TIGHT_CAP);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let newest = id(1);
        store.snapshot(&root, newest).unwrap();

        assert!(holds(&store, ALPHA), "{KEPT_MSG}");
        assert!(holds(&store, BETA), "{KEPT_MSG}");
        assert!(store.has_session_start(), "{KEPT_MSG}");
        assert!(store.has_checkpoint(newest), "{KEPT_MSG}");
    }

    #[test]
    fn garbage_is_collected_only_under_cap_pressure() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let roomy = store_in(&temp);
        let checkpoint = id(1);
        roomy.snapshot(&root, checkpoint).unwrap();
        write(&root, "file.txt", BETA);
        roomy.snapshot(&root, checkpoint).unwrap();
        assert!(holds(&roomy, ALPHA), "{UNPRESSED_MSG}");

        let tight = store_in(&temp).with_cap(TIGHT_CAP);
        tight
            .enforce_cap(
                &tight.open_repository().unwrap(),
                0,
                SnapshotKey::Checkpoint(checkpoint),
            )
            .unwrap();
        assert!(!holds(&tight, ALPHA), "{PRESSED_MSG}");
        assert!(holds(&tight, BETA), "{PRESSED_MSG}");
    }

    #[test]
    fn collection_keeps_what_the_unrevert_record_needs() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp).with_cap(TIGHT_CAP);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();

        store.snapshot(&root, id(2)).unwrap();

        assert!(!store.has_checkpoint(source), "{UNREVERT_KEPT_MSG}");
        assert!(holds(&store, BETA), "{UNREVERT_KEPT_MSG}");
        store.unrevert(&root).unwrap();
        assert_eq!(read(&root, "file.txt"), BETA, "{UNREVERT_KEPT_MSG}");
    }

    #[test]
    fn eviction_weighs_every_session_of_the_workspace_oldest_first() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let first = session_store(&temp, SESSION);
        let second = session_store(&temp, OTHER_SESSION);
        first.snapshot_session_start(&root).unwrap();
        second.snapshot_session_start(&root).unwrap();
        for (store, sequence) in [(&first, 1), (&second, 2), (&first, 3), (&second, 4)] {
            write(&root, "file.txt", sequence.to_string());
            store.snapshot(&root, id(sequence)).unwrap();
        }
        assert_eq!(
            evictable_checkpoints(first.sessions_dir(), KEY, Path::new("")),
            [
                first.pointer_path(SnapshotKey::Checkpoint(id(1))),
                second.pointer_path(SnapshotKey::Checkpoint(id(2))),
            ],
            "{SESSIONS_MSG}"
        );

        write(&root, "file.txt", ALPHA);
        session_store(&temp, OTHER_SESSION)
            .with_cap(TIGHT_CAP)
            .snapshot(&root, id(5))
            .unwrap();

        for (store, sequence) in [(&first, 1), (&second, 2), (&second, 4)] {
            assert!(!store.has_checkpoint(id(sequence)), "{SESSIONS_MSG}");
        }
        for (store, sequence) in [(&first, 3), (&second, 5)] {
            assert!(store.has_checkpoint(id(sequence)), "{KEPT_MSG}");
        }
        assert!(
            first.has_session_start() && second.has_session_start(),
            "{KEPT_MSG}"
        );
    }

    /// A store that cannot be collected is left for its next capture over the
    /// cap, rather than keeping every other workspace's garbage on disk.
    #[test_case(false ; "alone")]
    #[test_case(true  ; "beside_a_store_that_cannot_be_collected")]
    fn collecting_after_a_session_is_deleted_frees_only_what_it_alone_named(
        with_broken_store: bool,
    ) {
        let (temp, root) = setup();
        write(&root, "shared.txt", ALPHA);
        write(&root, "unique.txt", BETA);
        let deleted = session_store(&temp, SESSION);
        deleted.snapshot_session_start(&root).unwrap();
        fs::remove_file(root.join("unique.txt")).unwrap();
        let kept = session_store(&temp, OTHER_SESSION);
        kept.snapshot_session_start(&root).unwrap();
        fs::remove_dir_all(deleted.dir.parent().unwrap()).unwrap();
        if with_broken_store {
            let broken = state_root(&temp)
                .join(WORKSPACE_SNAPSHOTS_DIR)
                .join(OTHER_KEY);
            write(&broken, OBJECTS_DIR, ALPHA);
        }

        let freed = collect_garbage(&StateDir::from_path(state_root(&temp))).unwrap();

        assert!(freed > 0, "{UNIQUE_MSG}");
        assert!(!holds(&kept, BETA), "{UNIQUE_MSG}");
        assert!(holds(&kept, ALPHA), "{UNIQUE_MSG}");
        assert_eq!(
            paths(&kept, SnapshotKey::SessionStart),
            ["shared.txt"],
            "{UNIQUE_MSG}"
        );
    }

    /// Read as empty, such a directory would free every object its pointers
    /// name. A file in its place fails the listing as an unreadable directory
    /// would, whoever runs the test.
    fn make_unlistable(dir: &Path) {
        fs::rename(dir, dir.with_extension(MOVED_EXTENSION)).unwrap();
        fs::write(dir, "").unwrap();
    }

    #[test_case(true ; "the_sessions_directory")]
    #[test_case(false ; "a_checkpoints_directory")]
    fn collection_keeps_what_a_directory_it_cannot_list_may_name(sessions_directory: bool) {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        store.snapshot(&root, id(1)).unwrap();
        make_unlistable(&if sessions_directory {
            state_root(&temp).join(SESSION_SNAPSHOTS_DIR)
        } else {
            checkpoints_dir(&store.dir)
        });

        collect_garbage(&StateDir::from_path(state_root(&temp))).unwrap();

        assert!(holds(&store, ALPHA), "{UNLISTABLE_MSG}");
        assert!(holds(&store, BETA), "{UNLISTABLE_MSG}");
    }

    /// Retention only keeps the store near its cap, so a capture whose
    /// collection cannot run still stands, and still frees nothing.
    #[test]
    fn a_capture_over_its_cap_keeps_what_a_directory_it_cannot_list_may_name() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let other = session_store(&temp, OTHER_SESSION);
        other.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        other.snapshot(&root, id(1)).unwrap();
        make_unlistable(&checkpoints_dir(&other.dir));
        write(&root, "file.txt", GAMMA);

        let captured = store_in(&temp)
            .with_cap(TIGHT_CAP)
            .snapshot_session_start(&root);

        assert!(captured.is_ok(), "{RETENTION_FAILURE_MSG}: {captured:?}");
        assert!(holds(&other, BETA), "{UNLISTABLE_MSG}");
    }
}
