//! Which files are unchanged since they were last read, kept as a git index file so a capture of
//! an unchanged tree costs a stat per file rather than a read.
//!
//! It is stricter than git's: an entry is trusted only when every stat field matches to the
//! nanosecond, and it is only recorded when the file had not changed for [`RACY_MARGIN`] before the
//! capture that read it started. Git trusts a file modified in the same timestamp tick as the index
//! write only after re-checking it; here such a file is simply not cached, so a write racing the
//! read can never hide behind an unchanged stamp.

use std::{
    fs::{self, Metadata},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gix::{
    bstr::{BStr, ByteSlice as _},
    index::{
        File as IndexFile, State,
        entry::{
            Flags, Mode, Stat,
            stat::{Options as StatOptions, Time},
        },
    },
};

use crate::{HASH_KIND, ObjectId, ObjectStore, StoreError, meta::within};

/// How long a file must have been unchanged before a capture starts for its stamp to be cached.
/// Covers coarse filesystem timestamps and small clock differences.
pub const RACY_MARGIN: Duration = Duration::from_secs(5);
pub(crate) const STAT_INDEX: &str = "stat-index";
pub(crate) const STAT_INDEX_TEMPORARY: &str = "stat-index.lock";
const EXACT: StatOptions = StatOptions {
    trust_ctime: true,
    check_stat: true,
    use_nsec: true,
    use_stdev: true,
};

/// A file's stat as a git index records it. Take it before reading the file, so that any write
/// after the stamp changes what the next capture sees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStamp(Stat);

impl FileStamp {
    #[must_use]
    pub fn of(metadata: &Metadata) -> Self {
        // Truncation to 32 bits is git's index format; it only ever widens what counts as changed.
        #[cfg(unix)]
        let stat = {
            use std::os::unix::fs::MetadataExt;
            Stat {
                mtime: time(metadata.mtime(), metadata.mtime_nsec()),
                ctime: time(metadata.ctime(), metadata.ctime_nsec()),
                dev: metadata.dev() as u32,
                ino: metadata.ino() as u32,
                uid: metadata.uid(),
                gid: metadata.gid(),
                size: metadata.size() as u32,
            }
        };
        #[cfg(not(unix))]
        let stat = Stat {
            mtime: metadata.modified().map(system_time).unwrap_or_default(),
            ctime: metadata.created().map(system_time).unwrap_or_default(),
            size: metadata.len() as u32,
            ..Stat::default()
        };
        Self(stat)
    }

    fn settled_before(&self, threshold: Time) -> bool {
        self.0.mtime.max(self.0.ctime) < threshold
    }
}

#[cfg(unix)]
fn time(seconds: i64, nanoseconds: i64) -> Time {
    Time {
        secs: seconds as u32,
        nsecs: nanoseconds as u32,
    }
}

/// Whether the file ends in the checksum of everything before it. gix skips verification for a
/// null checksum and panics on a file shorter than one, and a crash can leave either behind.
fn intact(path: &Path) -> bool {
    let Ok(data) = fs::read(path) else {
        return false;
    };
    let Some((content, checksum)) = data
        .len()
        .checked_sub(HASH_KIND.len_in_bytes())
        .map(|split| data.split_at(split))
    else {
        return false;
    };
    let mut hasher = gix::hash::hasher(HASH_KIND);
    hasher.update(content);
    hasher
        .try_finalize()
        .is_ok_and(|expected| expected.as_bytes() == checksum)
}

fn system_time(at: SystemTime) -> Time {
    at.duration_since(UNIX_EPOCH)
        .map(|since| Time {
            secs: since.as_secs() as u32,
            nsecs: since.subsec_nanos(),
        })
        .unwrap_or_default()
}

/// The cache as last saved, plus what the current capture recorded.
pub struct StatCache {
    previous: Option<IndexFile>,
    recorded: Vec<(String, FileStamp, ObjectId)>,
}

impl StatCache {
    /// The blob last read from `path`, if the file still has exactly the stamp it had then.
    #[must_use]
    pub fn lookup(&self, path: &str, stamp: &FileStamp) -> Option<ObjectId> {
        let entry = self.previous.as_ref()?.entry_by_path(BStr::new(path))?;
        entry.stat.matches(&stamp.0, EXACT).then_some(entry.id)
    }

    /// Notes that `path`, stamped before it was read, held `oid`. Record hits as well as reads:
    /// saving replaces everything within the capture's scope with what was recorded.
    pub fn record(&mut self, path: String, stamp: FileStamp, oid: ObjectId) {
        self.recorded.push((path, stamp, oid));
    }

    pub(crate) fn previous_objects(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.previous
            .iter()
            .flat_map(|file| file.entries().iter().map(|entry| entry.id))
    }
}

impl ObjectStore {
    /// The saved cache, or an empty one when it is absent or unreadable: it is only a cache.
    #[must_use]
    pub fn stat_cache(&self) -> StatCache {
        let path = self.dir.join(STAT_INDEX);
        StatCache {
            previous: intact(&path)
                .then(|| IndexFile::at(&path, HASH_KIND, false, Default::default()).ok())
                .flatten(),
            recorded: Vec::new(),
        }
    }

    /// Saves what a capture of `scope` that started at `capture_started` recorded, keeping earlier
    /// entries outside that scope. Save only after [`Self::sync`]: the cache names objects.
    pub fn save_stat_cache(
        &self,
        cache: StatCache,
        scope: &str,
        capture_started: SystemTime,
    ) -> Result<(), StoreError> {
        let threshold = capture_started
            .checked_sub(RACY_MARGIN)
            .map(system_time)
            .unwrap_or_default();
        let mut state = State::new(HASH_KIND);
        if let Some(previous) = &cache.previous {
            for entry in previous.entries() {
                let path = entry.path(previous);
                if path.to_str().is_ok_and(|path| !within(scope, path)) {
                    state.dangerously_push_entry(
                        entry.stat,
                        entry.id,
                        Flags::empty(),
                        Mode::FILE,
                        path,
                    );
                }
            }
        }
        for (path, stamp, oid) in &cache.recorded {
            if within(scope, path) && stamp.settled_before(threshold) {
                state.dangerously_push_entry(
                    stamp.0,
                    *oid,
                    Flags::empty(),
                    Mode::FILE,
                    BStr::new(path),
                );
            }
        }
        state.sort_entries();
        let mut bytes = Vec::new();
        IndexFile::from_state(state, self.dir.join(STAT_INDEX))
            .write_to(&mut bytes, Default::default())
            .map_err(|error| StoreError::Io(std::io::Error::other(error)))?;
        self.replace_file(STAT_INDEX, STAT_INDEX_TEMPORARY, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::tests::{HELLO, store};

    const STALE: &str = "a stamp that differs in any field must not be trusted";
    const PATH: &str = "src/lib.rs";
    const STARTED_SECS: u32 = 1_000_000;
    const DAMAGED: &str = "a damaged cache must never name an object";
    /// The first entry's object id follows the 12-byte index header and ten 4-byte stat fields.
    const FIRST_OBJECT_ID_OFFSET: usize = 52;
    const TRUNCATED_BYTES: usize = 5;

    fn stamp(mtime_secs: u32) -> FileStamp {
        FileStamp(Stat {
            mtime: Time {
                secs: mtime_secs,
                nsecs: 7,
            },
            ctime: Time {
                secs: mtime_secs,
                nsecs: 7,
            },
            dev: 1,
            ino: 2,
            uid: 3,
            gid: 4,
            size: 5,
        })
    }

    fn started() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(STARTED_SECS.into())
    }

    fn saved(store: &ObjectStore, entries: &[(&str, FileStamp)], scope: &str) -> ObjectId {
        let oid = store.write_blob(HELLO).unwrap().oid;
        let mut cache = store.stat_cache();
        for (path, stamp) in entries {
            cache.record((*path).to_owned(), *stamp, oid);
        }
        store.save_stat_cache(cache, scope, started()).unwrap();
        oid
    }

    fn settled() -> FileStamp {
        stamp(STARTED_SECS - RACY_MARGIN.as_secs() as u32 - 1)
    }

    #[test]
    fn an_unchanged_settled_file_is_found_without_reading_it() {
        let (_dir, store) = store(false);
        let oid = saved(&store, &[(PATH, settled())], crate::ROOT_SCOPE);
        assert_eq!(store.stat_cache().lookup(PATH, &settled()), Some(oid));
        assert_eq!(store.stat_cache().lookup("other", &settled()), None);
    }

    #[test_case(|stat| stat.size += 1 ; "size")]
    #[test_case(|stat| stat.mtime.nsecs += 1 ; "modification nanoseconds")]
    #[test_case(|stat| stat.ctime.nsecs += 1 ; "a same size rewrite with its modification time reset")]
    #[test_case(|stat| stat.ino += 1 ; "a replaced inode")]
    #[test_case(|stat| stat.dev += 1 ; "another device")]
    #[test_case(|stat| stat.uid += 1 ; "another owner")]
    fn any_stat_difference_is_a_miss(change: fn(&mut Stat)) {
        let (_dir, store) = store(false);
        saved(&store, &[(PATH, settled())], crate::ROOT_SCOPE);
        let mut current = settled();
        change(&mut current.0);
        assert_eq!(store.stat_cache().lookup(PATH, &current), None, "{STALE}");
    }

    #[test_case(0 ; "modified as the capture started")]
    #[test_case(RACY_MARGIN.as_secs() as u32 ; "modified exactly the margin before")]
    fn a_file_modified_within_the_racy_margin_is_read_again(seconds_before_start: u32) {
        let (_dir, store) = store(false);
        let racy = stamp(STARTED_SECS - seconds_before_start);
        saved(&store, &[(PATH, racy)], crate::ROOT_SCOPE);
        assert_eq!(store.stat_cache().lookup(PATH, &racy), None);
    }

    #[test]
    fn saving_a_scoped_capture_keeps_entries_outside_the_scope_and_replaces_those_within() {
        let (_dir, store) = store(false);
        saved(
            &store,
            &[("docs/a", settled()), ("src/gone", settled())],
            crate::ROOT_SCOPE,
        );
        saved(&store, &[("src/new", settled())], "src");
        let cache = store.stat_cache();
        assert!(cache.lookup("docs/a", &settled()).is_some());
        assert!(cache.lookup("src/new", &settled()).is_some());
        assert!(cache.lookup("src/gone", &settled()).is_none());
    }

    fn damage_first_object_id(bytes: &mut [u8]) {
        bytes[FIRST_OBJECT_ID_OFFSET] ^= 1;
    }

    fn zero_checksum(bytes: &mut [u8]) {
        let checksum = bytes.len() - HASH_KIND.len_in_bytes();
        bytes[checksum..].fill(0);
    }

    #[test_case(|bytes| bytes.truncate(TRUNCATED_BYTES) ; "cut shorter than a checksum")]
    #[test_case(|bytes| damage_first_object_id(bytes) ; "with a damaged object id")]
    #[test_case(
        |bytes| {
            damage_first_object_id(bytes);
            zero_checksum(bytes);
        } ;
        "with a damaged object id and the zero checksum gix leaves unverified"
    )]
    fn a_damaged_cache_is_empty(damage: fn(&mut Vec<u8>)) {
        let (dir, store) = store(false);
        saved(&store, &[(PATH, settled())], crate::ROOT_SCOPE);
        let path = dir.path().join("store").join(STAT_INDEX);
        let mut bytes = fs::read(&path).unwrap();
        damage(&mut bytes);
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            store.stat_cache().lookup(PATH, &settled()),
            None,
            "{DAMAGED}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_file_stamp_matches_until_the_file_is_rewritten() {
        let (dir, store) = store(false);
        let path = dir.path().join("file");
        std::fs::write(&path, b"one").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3_600))
            .unwrap();
        let before = FileStamp::of(&std::fs::metadata(&path).unwrap());
        assert_eq!(before, FileStamp::of(&std::fs::metadata(&path).unwrap()));
        let oid = store.write_blob(b"one").unwrap().oid;
        let mut cache = store.stat_cache();
        cache.record("file".to_owned(), before, oid);
        let later = SystemTime::now() + RACY_MARGIN + Duration::from_secs(1);
        store
            .save_stat_cache(cache, crate::ROOT_SCOPE, later)
            .unwrap();
        assert_eq!(store.stat_cache().lookup("file", &before), Some(oid));
        std::fs::write(&path, b"two").unwrap();
        let after = FileStamp::of(&std::fs::metadata(&path).unwrap());
        assert_eq!(store.stat_cache().lookup("file", &after), None, "{STALE}");
    }
}
