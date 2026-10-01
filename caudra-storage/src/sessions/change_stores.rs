//! The local change record stores beneath a state directory. Their engine
//! lives in a crate above this one, so that crate registers its access here
//! once per process, the way [`super::set_eager_load_limit`] arrives: release
//! jobs, the sweep, and the storage views all reach the stores through it.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use caudra_workspace::{HolderSummary, RecordHolder};
use serde::Serialize;
use tracing::warn;

use crate::StateDir;
use crate::id::CaudraId;
use crate::projects::workspace_key;

static REGISTERED: OnceLock<Arc<dyn ChangeStores>> = OnceLock::new();

/// What one store keeps on disk, apart from who holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StoreUsage {
    pub bytes: u64,
    pub objects: u64,
    pub records: u32,
    pub open_records: u32,
    pub pending_reverts: u32,
}

impl StoreUsage {
    /// Nothing a session could still revert with, or is reverting with.
    pub fn keeps_nothing(&self) -> bool {
        self.records == 0 && self.open_records == 0 && self.pending_reverts == 0
    }
}

/// The change stores beneath any state directory, opened without the
/// workspaces they record.
pub trait ChangeStores: Send + Sync {
    /// The key of every store beneath `state`.
    fn keys(&self, state: &StateDir) -> io::Result<Vec<String>>;
    fn usage(&self, state: &StateDir, key: &str) -> io::Result<StoreUsage>;
    /// Every holder of one store, across every page.
    fn holders(&self, state: &StateDir, key: &str) -> io::Result<Vec<HolderSummary>>;
    /// Drops every hold `holder` has in one store. A record nobody else holds
    /// is deleted, and the holder's pending reverts are acknowledged.
    fn release(&self, state: &StateDir, key: &str, holder: &RecordHolder) -> io::Result<()>;
    /// The bytes cleaning one store down to `retention` reclaims, carried out
    /// unless `dry_run`. A store with nothing to clean is left untouched.
    fn clean_up(
        &self,
        state: &StateDir,
        key: &str,
        retention: NonZeroU64,
        dry_run: bool,
    ) -> io::Result<u64>;
}

/// One store, as the storage views list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreSummary {
    pub key: String,
    /// The directory whose changes the store records, when some session's
    /// directory still resolves to it.
    pub workspace: Option<PathBuf>,
    #[serde(flatten)]
    pub usage: StoreUsage,
    pub holders: Vec<HolderSummary>,
    /// No holder has a session in the database.
    pub orphaned: bool,
}

/// The first registration wins, so a process reaches one set of stores.
pub fn register_change_stores(stores: Arc<dyn ChangeStores>) {
    if REGISTERED.set(stores).is_err() {
        warn!("change stores were already registered; keeping the first");
    }
}

pub fn registered_change_stores() -> Option<Arc<dyn ChangeStores>> {
    REGISTERED.get().cloned()
}

/// Every store beneath `state`, largest first. `sessions` are the sessions
/// the database knows with their directories, which decide whether a store is
/// orphaned and which directory it records.
pub fn store_summaries(
    stores: &dyn ChangeStores,
    state: &StateDir,
    sessions: &[(CaudraId, String)],
) -> io::Result<Vec<StoreSummary>> {
    let known: HashSet<String> = sessions.iter().map(|(id, _)| id.to_string()).collect();
    let workspaces = workspaces(sessions);
    let mut summaries = stores
        .keys(state)?
        .into_iter()
        .map(|key| {
            let holders = stores.holders(state, &key).map_err(in_store(&key))?;
            Ok(StoreSummary {
                usage: stores.usage(state, &key).map_err(in_store(&key))?,
                orphaned: !holders
                    .iter()
                    .any(|summary| known.contains(summary.holder.as_str())),
                holders,
                workspace: workspaces.get(&key).cloned(),
                key,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    summaries.sort_by_key(|summary| Reverse(summary.usage.bytes));
    Ok(summaries)
}

/// The store key of every session directory that still exists, mapped to
/// that directory.
fn workspaces(sessions: &[(CaudraId, String)]) -> HashMap<String, PathBuf> {
    sessions
        .iter()
        .map(|(_, cwd)| cwd.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|cwd| {
            let root = fs::canonicalize(cwd).ok()?;
            Some((workspace_key(&root).ok()?, root))
        })
        .collect()
}

/// What cleaning every store beneath `state` down to `retention` would
/// reclaim.
pub fn reclaimable_bytes(
    stores: &dyn ChangeStores,
    state: &StateDir,
    retention: NonZeroU64,
) -> io::Result<u64> {
    stores.keys(state)?.iter().try_fold(0_u64, |total, key| {
        let bytes = stores
            .clean_up(state, key, retention, true)
            .map_err(in_store(key))?;
        Ok(total.saturating_add(bytes))
    })
}

/// Drops every hold `holder` has in every store beneath `state`. A store that
/// fails does not stop the others; the first failure is returned.
pub(super) fn release_everywhere(
    stores: &dyn ChangeStores,
    state: &StateDir,
    holder: CaudraId,
) -> io::Result<()> {
    let holder = RecordHolder::new(holder.to_string()).map_err(io::Error::other)?;
    let mut failure = None;
    for key in stores.keys(state)? {
        if let Err(error) = stores.release(state, &key, &holder) {
            failure.get_or_insert(in_store(&key)(error));
        }
    }
    failure.map_or(Ok(()), Err)
}

fn in_store(key: &str) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |error| io::Error::new(error.kind(), format!("change store {key}: {error}"))
}

#[cfg(test)]
pub(super) mod fake {
    use std::collections::BTreeMap;
    use std::io;
    use std::num::NonZeroU64;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use caudra_workspace::{HolderSummary, RecordHolder};

    use super::{ChangeStores, StoreUsage};
    use crate::StateDir;

    pub(in crate::sessions) const REFUSED: &str = "the fake store refuses";

    /// One store held in memory.
    #[derive(Debug, Clone, Default)]
    pub(in crate::sessions) struct FakeStore {
        pub holders: Vec<String>,
        pub usage: StoreUsage,
        pub reclaimable: u64,
        /// The retention of every cleanup carried out.
        pub cleaned_to: Vec<u64>,
    }

    /// Stores held in memory. While `refusing`, every release fails.
    #[derive(Default)]
    pub(in crate::sessions) struct FakeStores {
        pub stores: Mutex<BTreeMap<String, FakeStore>>,
        pub refusing: AtomicBool,
    }

    impl FakeStores {
        pub fn with<K: Into<String>>(stores: impl IntoIterator<Item = (K, FakeStore)>) -> Self {
            Self {
                stores: Mutex::new(
                    stores
                        .into_iter()
                        .map(|(key, store)| (key.into(), store))
                        .collect(),
                ),
                refusing: AtomicBool::default(),
            }
        }

        pub fn store(&self, key: &str) -> FakeStore {
            self.stores.lock().unwrap()[key].clone()
        }

        fn with_store<T>(
            &self,
            key: &str,
            read: impl FnOnce(&mut FakeStore) -> T,
        ) -> io::Result<T> {
            self.stores
                .lock()
                .unwrap()
                .get_mut(key)
                .map(read)
                .ok_or_else(|| io::ErrorKind::NotFound.into())
        }
    }

    impl ChangeStores for FakeStores {
        fn keys(&self, _state: &StateDir) -> io::Result<Vec<String>> {
            Ok(self.stores.lock().unwrap().keys().cloned().collect())
        }

        fn usage(&self, _state: &StateDir, key: &str) -> io::Result<StoreUsage> {
            self.with_store(key, |store| store.usage.clone())
        }

        fn holders(&self, _state: &StateDir, key: &str) -> io::Result<Vec<HolderSummary>> {
            self.with_store(key, |store| {
                store
                    .holders
                    .iter()
                    .map(|holder| HolderSummary {
                        holder: RecordHolder::new(holder.as_str()).unwrap(),
                        records: 1,
                        open_records: 0,
                        pending_reverts: 0,
                    })
                    .collect()
            })
        }

        fn release(&self, _state: &StateDir, key: &str, holder: &RecordHolder) -> io::Result<()> {
            if self.refusing.load(Ordering::SeqCst) {
                return Err(io::Error::other(REFUSED));
            }
            self.with_store(key, |store| {
                store.holders.retain(|held| held != holder.as_str());
            })
        }

        fn clean_up(
            &self,
            _state: &StateDir,
            key: &str,
            retention: NonZeroU64,
            dry_run: bool,
        ) -> io::Result<u64> {
            self.with_store(key, |store| {
                if !dry_run {
                    store.cleaned_to.push(retention.get());
                }
                store.reclaimable
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::fake::{FakeStore, FakeStores};
    use super::*;

    const ABANDONED: &str = "abandoned-workspace-key";
    const HELD: &str = "held-workspace-key";
    const UNHELD: &str = "unheld-workspace-key";
    const SMALLEST: u64 = 1;
    const MIDDLE: u64 = 2;
    const LARGEST: u64 = 3;
    const HELD_RECLAIMABLE: u64 = 5;
    const UNHELD_RECLAIMABLE: u64 = 7;
    const ORPHAN_RULE: &str =
        "a store is orphaned exactly when no holder has a session in the database, largest first";
    const PREVIEW_ONLY: &str = "the reclaimable total sums every store and cleans none";
    const NAMED_WORKSPACE: &str =
        "a store names the directory its key hashes, once a session's directory resolves to it";
    const GONE: &str = "/a-workspace-that-is-gone";

    fn store(bytes: u64, holders: &[CaudraId], reclaimable: u64) -> FakeStore {
        FakeStore {
            holders: holders.iter().map(CaudraId::to_string).collect(),
            usage: StoreUsage {
                bytes,
                ..StoreUsage::default()
            },
            reclaimable,
            ..FakeStore::default()
        }
    }

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, state_dir)
    }

    #[test]
    fn a_store_is_orphaned_when_no_holder_has_a_session() {
        let (_temp, state) = state_dir();
        let live = CaudraId::generate();
        let forgotten = CaudraId::generate();
        let stores = FakeStores::with([
            (ABANDONED, store(SMALLEST, &[forgotten], 0)),
            (HELD, store(LARGEST, &[forgotten, live], 0)),
            (UNHELD, store(MIDDLE, &[], 0)),
        ]);

        let summaries = store_summaries(&stores, &state, &[(live, GONE.to_owned())]).unwrap();

        assert_eq!(
            summaries
                .iter()
                .map(|summary| (summary.key.as_str(), summary.orphaned))
                .collect::<Vec<_>>(),
            [(HELD, false), (UNHELD, true), (ABANDONED, true)],
            "{ORPHAN_RULE}"
        );
    }

    #[test]
    fn a_store_names_the_directory_its_key_hashes() {
        let (_temp, state) = state_dir();
        let workspace = TempDir::new().unwrap();
        let root = fs::canonicalize(workspace.path()).unwrap();
        let key = workspace_key(&root).unwrap();
        let stores = FakeStores::with([
            (key.clone(), store(LARGEST, &[], 0)),
            (UNHELD.to_owned(), store(SMALLEST, &[], 0)),
        ]);
        let sessions = [
            (CaudraId::generate(), workspace.path().display().to_string()),
            (CaudraId::generate(), GONE.to_owned()),
        ];

        let summaries = store_summaries(&stores, &state, &sessions).unwrap();

        assert_eq!(
            summaries
                .iter()
                .map(|summary| (summary.key.as_str(), summary.workspace.as_deref()))
                .collect::<Vec<_>>(),
            [(key.as_str(), Some(root.as_path())), (UNHELD, None)],
            "{NAMED_WORKSPACE}"
        );
    }

    #[test]
    fn the_reclaimable_total_previews_every_store_and_cleans_none() {
        let (_temp, state) = state_dir();
        let stores = FakeStores::with([
            (HELD, store(0, &[], HELD_RECLAIMABLE)),
            (UNHELD, store(0, &[], UNHELD_RECLAIMABLE)),
        ]);

        let total = reclaimable_bytes(&stores, &state, NonZeroU64::MIN).unwrap();

        assert_eq!(
            total,
            HELD_RECLAIMABLE + UNHELD_RECLAIMABLE,
            "{PREVIEW_ONLY}"
        );
        for key in [HELD, UNHELD] {
            assert!(stores.store(key).cleaned_to.is_empty(), "{PREVIEW_ONLY}");
        }
    }
}
