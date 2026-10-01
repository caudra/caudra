//! Retention and collection. Retention evicts the oldest records to keep the store within a
//! target, and collection deletes every object nothing names any more. Neither touches what an
//! open record, a pending revert or an open record's window still needs, and a store that cannot
//! say what that is collects nothing.

use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    time::Duration,
};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{CleanupPreview, CleanupSummary, RecordClientMetadata};
use workcell_snapshot_store::{GarbagePlan, SnapshotId, Usage};

use crate::{
    Inner, MAX_STORE_RECORDS, SnapshotError, check_cancelled,
    format::{StoredOpenRecord, StoredRecord, StoredState, count},
    snapshot::store_error,
    store::{OPEN, RECORDS},
    unix_ms,
};

/// Longer than any call may run, so an open record this old belongs to a call that is gone.
const STALE_OPEN_RECORD: Duration = Duration::from_secs(12 * 60 * 60);

/// Records from `from` on were committed while some open record ran, which rebases onto them
/// when it finishes.
pub(crate) struct Kept {
    from: Option<u64>,
}

/// Everything collection keeps, read in full or not at all.
struct Roots {
    open: Vec<StoredOpenRecord>,
    records: Vec<StoredRecord>,
    reverted: BTreeSet<u64>,
}

#[derive(Serialize)]
pub(crate) struct CleanupPlan {
    stale: Vec<String>,
    evicted: Vec<u64>,
    pub(crate) preview: CleanupPreview,
}

impl Kept {
    pub(crate) fn keeps(&self, seq: u64) -> bool {
        self.from.is_some_and(|from| seq >= from)
    }
}

impl Roots {
    fn kept(&self) -> Kept {
        Kept {
            from: self.open.iter().map(|open| open.opened_seq).min(),
        }
    }

    /// Records retention may evict: older than `protect_from`, outside every open record's
    /// window, and named by no pending revert. Tombstones first, then the rest oldest first.
    fn evictable(&self, protect_from: u64) -> (BTreeSet<u64>, Vec<u64>) {
        let kept = self.kept();
        let mut tombstones = BTreeSet::new();
        let mut held = Vec::new();
        for record in &self.records {
            if record.seq >= protect_from
                || kept.keeps(record.seq)
                || self.reverted.contains(&record.seq)
            {
                continue;
            }
            if record.holders.is_empty() {
                tombstones.insert(record.seq);
            } else {
                held.push(record.seq);
            }
        }
        (tombstones, held)
    }
}

impl CleanupPlan {
    pub(crate) fn retained_bytes(&self) -> usize {
        self.stale
            .iter()
            .map(String::capacity)
            .fold(size_of::<Self>(), usize::saturating_add)
            .saturating_add(self.evicted.capacity().saturating_mul(size_of::<u64>()))
    }
}

impl Inner {
    pub(crate) fn kept(&self) -> Result<Kept, SnapshotError> {
        Ok(Kept {
            from: self
                .store
                .open_records()?
                .iter()
                .map(|open| open.opened_seq)
                .min(),
        })
    }

    /// Keeps the store within `target_bytes` with room for another record: measures it, and when
    /// that is not enough collects what nothing names, evicting the oldest records first if even
    /// that does not suffice. Nothing from `protect_from` on is evicted.
    pub(crate) fn retain(
        &self,
        state: &mut StoredState,
        target_bytes: u64,
        protect_from: u64,
    ) -> Result<(), SnapshotError> {
        if state.bytes <= target_bytes && !crowded(state.records) {
            return Ok(());
        }
        self.measure(state)?;
        if state.bytes <= target_bytes && !crowded(state.records) {
            return Ok(());
        }
        let roots = self.roots()?;
        let evicted = self.plan_eviction(&roots, state.bytes, target_bytes, protect_from)?;
        self.evict(&roots, &evicted)?;
        self.collect(&roots, &evicted)?;
        self.measure(state)
    }

    /// Plans abandoning stale open records and evicting records until the store is within
    /// `retention_bytes`.
    pub(crate) fn plan_cleanup(&self, retention_bytes: u64) -> Result<CleanupPlan, SnapshotError> {
        let mut roots = self.roots()?;
        let now = unix_ms();
        let (stale, open) = roots
            .open
            .into_iter()
            .partition::<Vec<_>, _>(|open| is_stale(open, now));
        roots.open = open;
        let usage = self.store.usage()?;
        let evicted = self.plan_eviction(&roots, usage, retention_bytes, u64::MAX)?;
        let files = stale
            .iter()
            .map(|open| self.store.size(&self.store.open_path(&open.ticket)?))
            .chain(
                evicted
                    .iter()
                    .map(|seq| self.store.size(&self.store.record_path(*seq))),
            )
            .try_fold(0_u64, |total, size| {
                Ok::<_, SnapshotError>(total.saturating_add(size?))
            })?;
        let garbage = self.garbage(&roots, &evicted)?.usage();
        Ok(CleanupPlan {
            preview: CleanupPreview {
                stale_open_records: count(stale.len()),
                evicted_records: count(evicted.len()),
                reclaimable_bytes: garbage.bytes.saturating_add(files),
            },
            stale: stale.into_iter().map(|open| open.ticket).collect(),
            evicted: evicted.into_iter().collect(),
        })
    }

    /// Does what `plan` named, and only that, where it still applies, then collects everything
    /// nothing names any more.
    pub(crate) fn execute_cleanup(
        &self,
        plan: &CleanupPlan,
        token: &CancellationToken,
    ) -> Result<CleanupSummary, SnapshotError> {
        check_cancelled(token)?;
        let now = unix_ms();
        let mut abandoned = 0_usize;
        let mut reclaimed = 0_u64;
        for ticket in &plan.stale {
            let open = match self.store.open_record(ticket) {
                Err(SnapshotError::NotFound) => continue,
                open => open?,
            };
            let path = self.store.open_path(ticket)?;
            let size = self.store.size(&path)?;
            if is_stale(&open, now) && self.store.remove(&path)? {
                abandoned += 1;
                reclaimed = reclaimed.saturating_add(size);
            }
        }
        if abandoned > 0 {
            self.store.sync(OPEN)?;
        }
        let roots = self.roots()?;
        let (tombstones, held) = roots.evictable(u64::MAX);
        let evictable = tombstones.into_iter().chain(held).collect::<BTreeSet<_>>();
        let evicted = plan
            .evicted
            .iter()
            .copied()
            .filter(|seq| evictable.contains(seq))
            .collect::<BTreeSet<_>>();
        for seq in &evicted {
            reclaimed = reclaimed.saturating_add(self.store.size(&self.store.record_path(*seq))?);
        }
        self.evict(&roots, &evicted)?;
        let collected = self.collect(&roots, &evicted)?;
        let mut state = self.store.state()?;
        self.measure(&mut state)?;
        Ok(CleanupSummary {
            abandoned_open_records: count(abandoned),
            evicted_records: count(evicted.len()),
            deleted_objects: u32::try_from(collected.objects).unwrap_or(u32::MAX),
            reclaimed_bytes: reclaimed.saturating_add(collected.bytes),
        })
    }

    /// Everything collection must keep. Anything that cannot be read fails the listing, so no
    /// object it names is ever mistaken for garbage.
    fn roots(&self) -> Result<Roots, SnapshotError> {
        Ok(Roots {
            open: self.store.open_records()?,
            records: self.store.records()?,
            reverted: self.reverted()?,
        })
    }

    /// The fewest records to evict, oldest first, for the store to fit `target_bytes` with room
    /// for another record, or every evictable one when even that is not enough. Tombstones
    /// outside every window always go: nothing needs them.
    fn plan_eviction(
        &self,
        roots: &Roots,
        usage: u64,
        target_bytes: u64,
        protect_from: u64,
    ) -> Result<BTreeSet<u64>, SnapshotError> {
        let (tombstones, held) = roots.evictable(protect_from);
        let sizes = held
            .iter()
            .map(|seq| self.store.size(&self.store.record_path(*seq)))
            .collect::<Result<Vec<_>, _>>()?;
        let tombstone_bytes = tombstones.iter().try_fold(0_u64, |total, seq| {
            Ok::<_, SnapshotError>(
                total.saturating_add(self.store.size(&self.store.record_path(*seq))?),
            )
        })?;
        let evicting = |count: usize| {
            tombstones
                .iter()
                .chain(&held[..count])
                .copied()
                .collect::<BTreeSet<_>>()
        };
        let remaining = roots.records.len().saturating_sub(tombstones.len());
        let by_count = (remaining + 1)
            .saturating_sub(MAX_STORE_RECORDS)
            .min(held.len());
        let count = smallest_fitting(by_count, held.len(), |count| {
            let freed = self
                .garbage(roots, &evicting(count))?
                .usage()
                .bytes
                .saturating_add(tombstone_bytes)
                .saturating_add(sizes[..count].iter().sum::<u64>());
            Ok(usage.saturating_sub(freed) <= target_bytes)
        })?;
        Ok(evicting(count))
    }

    /// Deletes `evicted`, first noting for each holder the newest record it lost.
    fn evict(&self, roots: &Roots, evicted: &BTreeSet<u64>) -> Result<(), SnapshotError> {
        if evicted.is_empty() {
            return Ok(());
        }
        let mut newest = BTreeMap::<&str, (u64, &RecordClientMetadata)>::new();
        for record in roots
            .records
            .iter()
            .filter(|record| evicted.contains(&record.seq))
        {
            for holder in &record.holders {
                newest.insert(holder, (record.seq, &record.client));
            }
        }
        for (holder, (seq, client)) in newest {
            self.store.mark_evicted(holder, seq, client)?;
        }
        for seq in evicted {
            self.store.remove(&self.store.record_path(*seq))?;
        }
        self.store.sync(RECORDS)?;
        tracing::info!(
            records = evicted.len(),
            "workspace change retention evicted records"
        );
        Ok(())
    }

    fn collect(&self, roots: &Roots, evicted: &BTreeSet<u64>) -> Result<Usage, SnapshotError> {
        let plan = self.garbage(roots, evicted)?;
        self.store.objects().collect(&plan).map_err(store_error)
    }

    /// What collection deletes once `evicted` is gone: every object no open record's tree, no
    /// other record and not the stat cache names.
    fn garbage(
        &self,
        roots: &Roots,
        evicted: &BTreeSet<u64>,
    ) -> Result<GarbagePlan, SnapshotError> {
        let trees = roots
            .open
            .iter()
            .map(StoredOpenRecord::before)
            .collect::<Result<Vec<SnapshotId>, _>>()?;
        let objects = roots
            .records
            .iter()
            .filter(|record| !evicted.contains(&record.seq))
            .flat_map(StoredRecord::objects)
            .collect::<Vec<_>>();
        self.store
            .objects()
            .garbage_keeping(&trees, &objects)
            .map_err(store_error)
    }

    /// Replaces the state's estimates with what the store holds.
    fn measure(&self, state: &mut StoredState) -> Result<(), SnapshotError> {
        state.bytes = self.store.usage()?;
        state.records = u64::try_from(self.store.seqs()?.len()).unwrap_or(u64::MAX);
        self.store.write_state(state)
    }
}

/// Whether the store has no room for another record.
fn crowded(records: u64) -> bool {
    usize::try_from(records).unwrap_or(usize::MAX) >= MAX_STORE_RECORDS
}

fn is_stale(open: &StoredOpenRecord, now: u64) -> bool {
    u128::from(now.saturating_sub(open.opened_at_unix_ms)) > STALE_OPEN_RECORD.as_millis()
}

/// The smallest count in `low..=high` that fits, assuming fitting only gets easier as the count
/// grows, or `high` when none does. Gallops from `low`, so a small answer costs few probes.
fn smallest_fitting(
    low: usize,
    high: usize,
    mut fits: impl FnMut(usize) -> Result<bool, SnapshotError>,
) -> Result<usize, SnapshotError> {
    if low >= high || fits(low)? {
        return Ok(low.min(high));
    }
    let (mut failing, mut step) = (low, 1_usize);
    let mut fitting = loop {
        let probe = failing.saturating_add(step).min(high);
        if probe == high || fits(probe)? {
            break probe;
        }
        failing = probe;
        step = step.saturating_mul(2);
    };
    while fitting - failing > 1 {
        let middle = failing + (fitting - failing) / 2;
        if fits(middle)? {
            fitting = middle;
        } else {
            failing = middle;
        }
    }
    Ok(fitting)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(0, 10, 0 ; "when nothing needs evicting")]
    #[test_case(0, 10, 1 ; "when one record suffices")]
    #[test_case(0, 10, 7 ; "between two gallops")]
    #[test_case(0, 10, 10 ; "at the last candidate")]
    #[test_case(3, 10, 5 ; "above a floor")]
    #[test_case(4, 4, 4 ; "with nothing to choose")]
    fn the_smallest_fitting_count_is_found(low: usize, high: usize, answer: usize) {
        let mut probes = Vec::new();

        let found = smallest_fitting(low, high, |count| {
            probes.push(count);
            Ok(count >= answer)
        })
        .unwrap();

        assert_eq!(found, answer);
        assert!(probes.iter().all(|probe| (low..=high).contains(probe)));
    }

    #[test]
    fn every_candidate_goes_when_none_fits() {
        assert_eq!(smallest_fitting(0, 6, |_| Ok(false)).unwrap(), 6);
    }

    #[test]
    fn a_failed_probe_fails_the_search() {
        assert_eq!(
            smallest_fitting(0, 6, |_| Err(SnapshotError::UnhealthyStorage)).unwrap_err(),
            SnapshotError::UnhealthyStorage
        );
    }
}
