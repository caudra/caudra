//! Collection. gix has no gc or prune, so this is a mark-and-sweep over loose objects: everything
//! reachable from the caller's roots or named by the stat cache is live, everything else is
//! garbage, and so is whatever writes left staged. Callers hold the lock every writer holds, so no
//! grace period protects new objects.

use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    HASH_KIND, ObjectId, ObjectStore, SnapshotId, StoreError, TEMPORARY_PREFIX, parse_oid, remove,
    stat_cache::{STAT_INDEX, STAT_INDEX_TEMPORARY},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub objects: u64,
    pub bytes: u64,
}

/// What a collection would delete, computed without deleting anything.
#[derive(Debug, Default)]
pub struct GarbagePlan {
    objects: Vec<(ObjectId, u64)>,
    temporaries: Vec<(PathBuf, u64)>,
}

impl GarbagePlan {
    #[must_use]
    pub fn usage(&self) -> Usage {
        Usage {
            objects: self.objects.len() as u64,
            bytes: self
                .objects
                .iter()
                .map(|(_, bytes)| bytes)
                .chain(self.temporaries.iter().map(|(_, bytes)| bytes))
                .sum(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty() && self.temporaries.is_empty()
    }

    /// Names exactly the set of objects the plan deletes, so a caller can confirm a plan it
    /// previewed is still the plan it would execute.
    pub fn digest(&self) -> Result<ObjectId, StoreError> {
        let mut hasher = gix::hash::hasher(HASH_KIND);
        for (oid, _) in &self.objects {
            hasher.update(oid.as_bytes());
        }
        hasher.try_finalize().map_err(|_| StoreError::Collision)
    }
}

impl ObjectStore {
    /// Plans a collection that keeps every object reachable from `roots` or named by the stat
    /// cache. A root that is missing protects nothing; a root that cannot be read aborts the plan,
    /// because what it reaches is unknown.
    pub fn garbage<'a>(
        &self,
        roots: impl IntoIterator<Item = &'a SnapshotId>,
    ) -> Result<GarbagePlan, StoreError> {
        self.garbage_keeping(roots, std::iter::empty())
    }

    /// [`Self::garbage`], also keeping `objects` no root reaches, such as what a restore saved to
    /// undo itself.
    pub fn garbage_keeping<'a>(
        &self,
        roots: impl IntoIterator<Item = &'a SnapshotId>,
        objects: impl IntoIterator<Item = &'a ObjectId>,
    ) -> Result<GarbagePlan, StoreError> {
        let mut live: HashSet<ObjectId> = self.stat_cache().previous_objects().collect();
        live.extend(objects);
        for root in roots {
            self.mark_snapshot(root, &mut live)?;
        }
        let mut plan = GarbagePlan::default();
        for fan_out in fs::read_dir(self.loose.path())? {
            let fan_out = fan_out?;
            let name = fan_out.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(TEMPORARY_PREFIX) {
                plan.temporaries
                    .push((fan_out.path(), fan_out.metadata()?.len()));
                continue;
            }
            if !fan_out.file_type()?.is_dir() {
                continue;
            }
            for object in fs::read_dir(fan_out.path())? {
                let object = object?;
                let Some(oid) = object
                    .file_name()
                    .to_str()
                    .and_then(|rest| parse_oid(&format!("{name}{rest}")))
                else {
                    continue;
                };
                if !live.contains(&oid) {
                    plan.objects.push((oid, object.metadata()?.len()));
                }
            }
        }
        plan.objects.sort_unstable();
        plan.temporaries.sort_unstable();
        Ok(plan)
    }

    /// Deletes what `plan` names and returns what was freed. Objects already gone count as freed.
    pub fn collect(&self, plan: &GarbagePlan) -> Result<Usage, StoreError> {
        let mut freed = Usage::default();
        for (oid, bytes) in &plan.objects {
            remove(&self.loose.object_path(oid))?;
            freed.objects += 1;
            freed.bytes += bytes;
        }
        let removal = plan.temporaries.iter().try_for_each(|(temporary, bytes)| {
            remove(temporary)?;
            freed.bytes += bytes;
            Ok::<_, StoreError>(())
        });
        // What this handle staged is among them, and a sync must not look for it.
        self.pending().staged.retain(|_, staged| {
            plan.temporaries
                .binary_search_by(|(temporary, _)| temporary.cmp(staged))
                .is_err()
        });
        removal?;
        Ok(freed)
    }

    /// The bytes on disk of every object, whatever writes left staged, and the stat cache.
    pub fn usage(&self) -> Result<Usage, StoreError> {
        let mut usage = Usage {
            objects: 0,
            bytes: size(&self.dir.join(STAT_INDEX))? + size(&self.dir.join(STAT_INDEX_TEMPORARY))?,
        };
        for entry in fs::read_dir(self.loose.path())? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                usage.bytes += metadata.len();
            }
            if !metadata.is_dir() {
                continue;
            }
            for object in fs::read_dir(entry.path())? {
                usage.objects += 1;
                usage.bytes += object?.metadata()?.len();
            }
        }
        Ok(usage)
    }

    fn mark_snapshot(
        &self,
        id: &SnapshotId,
        live: &mut HashSet<ObjectId>,
    ) -> Result<(), StoreError> {
        if !live.insert(id.oid()) {
            return Ok(());
        }
        let (files, meta) = match self.snapshot_parts(id) {
            Err(StoreError::Missing(_)) => return Ok(()),
            parts => parts?,
        };
        live.insert(meta);
        self.mark_tree(files, live)
    }

    fn mark_tree(&self, tree: ObjectId, live: &mut HashSet<ObjectId>) -> Result<(), StoreError> {
        let mut pending = vec![tree];
        while let Some(tree) = pending.pop() {
            if !live.insert(tree) {
                continue;
            }
            let nodes = match self.read_tree(&tree) {
                Err(StoreError::Missing(_)) => continue,
                nodes => nodes?,
            };
            for node in nodes {
                if node.is_tree() {
                    pending.push(node.oid);
                } else {
                    live.insert(node.oid);
                }
            }
        }
        Ok(())
    }
}

fn size(path: &Path) -> Result<u64, StoreError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::{
        EntryKind, RACY_MARGIN, ROOT_SCOPE, SnapshotId,
        tests::store,
        tree::tests::{entry, meta},
    };

    const LIVE: &str = "an object a root or the stat cache names must survive collection";
    const LEFTOVER: &[u8] = b"what a crash left staged";

    fn snapshot(store: &ObjectStore, files: &[(&str, &str)]) -> SnapshotId {
        let entries: Vec<_> = files
            .iter()
            .map(|(path, data)| entry(store, path, EntryKind::File, data))
            .collect();
        let id = store.build(&entries, &meta(ROOT_SCOPE, &entries)).unwrap();
        store.sync().unwrap();
        id
    }

    #[test]
    fn collection_keeps_what_roots_and_the_stat_cache_reach_and_sweeps_the_rest() {
        let (_dir, store) = store(false);
        let kept = snapshot(&store, &[("dir/kept", "k"), ("shared", "s")]);
        let dropped = snapshot(&store, &[("dir/dropped", "d"), ("shared", "s")]);
        let cached = store.write_blob(b"cached").unwrap().oid;
        let orphan = store.write_blob(b"orphan").unwrap().oid;
        store.sync().unwrap();
        let mut cache = store.stat_cache();
        cache.record(
            "cached".to_owned(),
            crate::FileStamp::of(&fs::metadata(store.dir()).unwrap()),
            cached,
        );
        store
            .save_stat_cache(cache, ROOT_SCOPE, SystemTime::now() + RACY_MARGIN * 2)
            .unwrap();
        let before = store.usage().unwrap();

        let plan = store.garbage([&kept]).unwrap();
        let planned = plan.usage();
        assert_eq!(
            plan.digest().unwrap(),
            store.garbage([&kept]).unwrap().digest().unwrap()
        );
        assert_eq!(store.collect(&plan).unwrap(), planned);

        let after = store.usage().unwrap();
        assert_eq!(after.objects, before.objects - planned.objects);
        assert!(store.entries(&kept).is_ok(), "{LIVE}");
        assert!(store.read_blob(&cached).is_ok(), "{LIVE}");
        assert!(matches!(
            store.read_blob(&orphan),
            Err(StoreError::Missing(_))
        ));
        assert!(matches!(
            store.entries(&dropped),
            Err(StoreError::Missing(_))
        ));
        assert!(store.garbage([&kept]).unwrap().is_empty());
    }

    #[test]
    fn collection_deletes_what_writes_left_staged_and_later_syncs_still_work() {
        let (_dir, store) = store(false);
        fs::write(
            store
                .loose
                .path()
                .join(format!("{TEMPORARY_PREFIX}crashed")),
            LEFTOVER,
        )
        .unwrap();
        store.write_blob(b"abandoned").unwrap();
        let staged = store.usage().unwrap();
        assert!(staged.bytes > LEFTOVER.len() as u64);

        let plan = store.garbage([]).unwrap();
        assert_eq!(plan.usage().bytes, staged.bytes);
        assert_eq!(store.collect(&plan).unwrap(), plan.usage());
        assert_eq!(store.usage().unwrap(), Usage::default());
        store.write_blob(b"kept").unwrap();
        store.sync().unwrap();
    }

    #[test]
    fn a_plan_digest_changes_with_what_it_would_delete() {
        let (_dir, store) = store(false);
        let empty = store.garbage([]).unwrap().digest().unwrap();
        store.write_blob(b"orphan").unwrap();
        store.sync().unwrap();
        assert_ne!(store.garbage([]).unwrap().digest().unwrap(), empty);
    }

    #[test]
    fn an_object_kept_by_name_survives_without_any_snapshot() {
        let (_dir, store) = store(false);
        let journaled = store.write_blob(b"before the restore").unwrap().oid;
        let orphan = store.write_blob(b"orphan").unwrap().oid;
        store.sync().unwrap();
        let plan = store.garbage_keeping([], [&journaled]).unwrap();
        store.collect(&plan).unwrap();
        assert!(store.read_blob(&journaled).is_ok(), "{LIVE}");
        assert!(matches!(
            store.read_blob(&orphan),
            Err(StoreError::Missing(_))
        ));
    }

    #[test]
    fn a_missing_root_protects_nothing() {
        let (_dir, store) = store(false);
        let root = snapshot(&store, &[("a", "a")]);
        let missing = SnapshotId(crate::blob_id(b"absent").unwrap());
        assert!(store.garbage([&root, &missing]).unwrap().is_empty());
        assert!(store.garbage([&missing]).unwrap().usage().objects > 0);
    }
}
