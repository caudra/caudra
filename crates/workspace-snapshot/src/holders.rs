//! Holders: who keeps each record. A record goes with its last holder, unless an open record's
//! window may still rebase onto it: then it stays, held by no one, until cleanup.

use std::collections::{BTreeMap, BTreeSet};

use workcell_host_contract::{
    ChangeInventory, HolderSummary, MAX_RECORD_PAGE_SIZE, RecordHolder, RecordListing, RecordPage,
    RecordState, ReleaseSelection, ReleaseSummary,
};

use crate::{
    Inner, SnapshotError,
    format::{count, holder},
    store::{EVICTED, OPEN, RECORDS, REVERTS},
};

impl Inner {
    /// Has `to` hold every record `from` holds, and inherit what retention evicted from `from`.
    /// Returns how many records `from` holds.
    pub(crate) fn hold(&self, from: &str, to: &str) -> Result<u32, SnapshotError> {
        let mut held = 0_u32;
        for header in self.store.headers()? {
            if !header.holders.contains(from) {
                continue;
            }
            held = held.saturating_add(1);
            if let Some(mut record) = self.store.record(header.seq)?
                && record.holders.insert(to.to_owned())
            {
                self.store.write_record(&record)?;
            }
        }
        if let Some(evicted) = self.store.evicted(from)? {
            self.store.mark_evicted(to, evicted.seq, &evicted.client)?;
        }
        Ok(held)
    }

    /// Drops `holder`'s hold on the selected records. Releasing everything settles the holder's
    /// pending reverts first; releasing a record a pending revert names is refused.
    pub(crate) fn release(
        &self,
        holder_id: &str,
        selection: &ReleaseSelection,
    ) -> Result<ReleaseSummary, SnapshotError> {
        let seqs = match selection {
            ReleaseSelection::All => {
                self.acknowledge(holder_id)?;
                self.store
                    .headers()?
                    .into_iter()
                    .filter(|header| header.holders.contains(holder_id))
                    .map(|header| header.seq)
                    .collect()
            }
            ReleaseSelection::Seqs(seqs) => {
                self.settle()?;
                let seqs = seqs.iter().copied().collect::<BTreeSet<_>>();
                if !self.reverted()?.is_disjoint(&seqs) {
                    return Err(SnapshotError::Conflict);
                }
                seqs
            }
        };
        let kept = self.kept()?;
        let mut summary = ReleaseSummary {
            released: 0,
            deleted: 0,
        };
        let mut removed = 0_u64;
        for seq in seqs {
            let Some(mut record) = self.store.record(seq)? else {
                continue;
            };
            if !record.holders.remove(holder_id) {
                continue;
            }
            summary.released = summary.released.saturating_add(1);
            if record.holders.is_empty() {
                summary.deleted = summary.deleted.saturating_add(1);
            }
            if !record.holders.is_empty() || kept.keeps(seq) {
                self.store.write_record(&record)?;
            } else if self.store.remove(&self.store.record_path(seq))? {
                removed += 1;
            }
        }
        if removed > 0 {
            self.store.sync(RECORDS)?;
            let mut state = self.store.state()?;
            state.records = state.records.saturating_sub(removed);
            self.store.write_state(&state)?;
        }
        if matches!(selection, ReleaseSelection::All)
            && self.store.remove(&self.store.evicted_path(holder_id))?
        {
            self.store.sync(EVICTED)?;
        }
        Ok(summary)
    }

    /// One page of `holder`'s records in seq order, reverted ones marked.
    pub(crate) fn records_of(
        &self,
        holder_id: &str,
        after_seq: Option<u64>,
        page_size: u32,
    ) -> Result<RecordPage, SnapshotError> {
        let page_size = page(page_size)?;
        let reverted = self.reverted()?;
        let mut records = Vec::new();
        let mut next_after_seq = None;
        for seq in self.store.seqs()? {
            if after_seq.is_some_and(|after| seq <= after) {
                continue;
            }
            let Some(header) = self.store.header(seq)? else {
                continue;
            };
            if !header.holders.contains(holder_id) {
                continue;
            }
            if records.len() == page_size {
                next_after_seq = records.last().map(|record: &RecordListing| record.seq);
                break;
            }
            records.push(RecordListing {
                seq,
                client: header.client,
                state: if reverted.contains(&seq) {
                    RecordState::Reverted
                } else {
                    RecordState::Applied
                },
                paths: count(header.changes.0),
                unrecorded: count(header.unrecorded.0),
            });
        }
        Ok(RecordPage {
            records,
            next_after_seq,
            evicted_through: self.store.evicted(holder_id)?.map(|evicted| evicted.client),
        })
    }

    /// One page of every holder with what it holds, in holder order.
    pub(crate) fn holders_page(
        &self,
        after: Option<&str>,
        page_size: u32,
    ) -> Result<(Vec<HolderSummary>, Option<RecordHolder>), SnapshotError> {
        let page_size = page(page_size)?;
        let mut holders = self
            .holder_summaries()?
            .into_values()
            .filter(|summary| after.is_none_or(|after| summary.holder.as_str() > after))
            .take(page_size.saturating_add(1))
            .collect::<Vec<_>>();
        let next_after = (holders.len() > page_size).then(|| {
            holders.truncate(page_size);
            holders[page_size - 1].holder.clone()
        });
        Ok((holders, next_after))
    }

    pub(crate) fn inventory(&self) -> Result<ChangeInventory, SnapshotError> {
        Ok(ChangeInventory {
            bytes: self.store.usage()?,
            objects: self.store.object_count()?,
            records: count(self.store.names(RECORDS)?.len()),
            open_records: count(self.store.names(OPEN)?.len()),
            pending_reverts: count(self.store.names(REVERTS)?.len()),
            holders: self.holder_summaries()?.into_values().collect(),
        })
    }

    fn holder_summaries(&self) -> Result<BTreeMap<String, HolderSummary>, SnapshotError> {
        let mut summaries = BTreeMap::new();
        for header in self.store.headers()? {
            for holder_id in &header.holders {
                let entry = summary(&mut summaries, holder_id)?;
                entry.records = entry.records.saturating_add(1);
            }
        }
        for open in self.store.open_records()? {
            let entry = summary(&mut summaries, open.request.holder.as_str())?;
            entry.open_records = entry.open_records.saturating_add(1);
        }
        for journal in self.store.journals()? {
            let entry = summary(&mut summaries, &journal.holder)?;
            entry.pending_reverts = entry.pending_reverts.saturating_add(1);
        }
        Ok(summaries)
    }
}

fn summary<'a>(
    summaries: &'a mut BTreeMap<String, HolderSummary>,
    holder_id: &str,
) -> Result<&'a mut HolderSummary, SnapshotError> {
    if !summaries.contains_key(holder_id) {
        summaries.insert(
            holder_id.to_owned(),
            HolderSummary {
                holder: holder(holder_id)?,
                records: 0,
                open_records: 0,
                pending_reverts: 0,
            },
        );
    }
    summaries
        .get_mut(holder_id)
        .ok_or(SnapshotError::OperationFailed)
}

fn page(page_size: u32) -> Result<usize, SnapshotError> {
    if page_size == 0 || page_size > MAX_RECORD_PAGE_SIZE {
        return Err(SnapshotError::InvalidRequest);
    }
    usize::try_from(page_size).map_err(|_| SnapshotError::InvalidRequest)
}
