//! Recording: a record's scope captured before and after its call and diffed, then rebased onto
//! the records other calls committed while it ran, so overlapping calls chain rather than overlap.

use std::collections::{BTreeMap, HashMap, HashSet};

use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    MAX_OPEN_RECORDS, MAX_RECORD_SCOPE_PATHS, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_FILES,
    MAX_SNAPSHOT_TOTAL_BYTES, OpenRecord, RecordRequest, RecordScope, RecordSummary, SnapshotLimit,
    UnrecordedReason,
};
use workcell_snapshot_store::{Change, Content, ROOT_SCOPE};

use crate::{
    Inner, MAX_STORE_RECORDS, SnapshotError,
    capture::{Widening, capture},
    format::{
        Blind, StoredChange, StoredContent, StoredOpenRecord, StoredRecord, StoredUnrecorded,
        encode,
    },
    identifier, quota_error,
    snapshot::{store_error, valid_path},
    store::OPEN,
    unix_ms,
};

type State = Option<Content>;

/// Refuses a request no record could honour, before anything is captured. Named paths must be
/// plain root-relative paths, so none reaches outside the root.
pub(crate) fn validate(request: &RecordRequest) -> Result<(), SnapshotError> {
    let limits = &request.limits;
    let files = usize::try_from(limits.max_files).unwrap_or(usize::MAX);
    let limits_valid = (1..=MAX_SNAPSHOT_FILES).contains(&files)
        && (1..=MAX_SNAPSHOT_FILE_BYTES).contains(&limits.max_file_bytes)
        && (1..=MAX_SNAPSHOT_TOTAL_BYTES).contains(&limits.max_total_bytes);
    let scope_valid = match &request.scope {
        RecordScope::Paths { paths } => {
            (1..=MAX_RECORD_SCOPE_PATHS).contains(&paths.len())
                && paths.iter().all(|path| valid_path(path))
        }
        RecordScope::Workspace { directory } => {
            directory.as_str() == ROOT_SCOPE || valid_path(directory)
        }
    };
    if limits_valid && scope_valid {
        Ok(())
    } else {
        Err(SnapshotError::InvalidRequest)
    }
}

impl Inner {
    /// Captures the scope before a call and keeps it as an open record, which survives restarts
    /// until it is finished or abandoned.
    pub(crate) fn begin(
        &self,
        request: RecordRequest,
        token: &CancellationToken,
    ) -> Result<String, SnapshotError> {
        let workspace = self.workspace()?;
        if self.store.names(OPEN)?.len() >= MAX_OPEN_RECORDS {
            return Err(quota_error(SnapshotLimit::OpenRecords, MAX_OPEN_RECORDS));
        }
        let mut state = self.store.state()?;
        let next_seq = state.next_seq;
        self.retain(&mut state, request.limits.max_total_bytes, next_seq)?;
        if usize::try_from(state.records).unwrap_or(usize::MAX) >= MAX_STORE_RECORDS {
            return Err(quota_error(SnapshotLimit::Records, MAX_STORE_RECORDS));
        }
        let captured = capture(
            &self.store,
            workspace,
            &request.scope,
            Widening::Decide,
            &request.limits,
            state.bytes,
            token,
        )?;
        let open = StoredOpenRecord::new(
            request,
            captured.widened,
            state.next_seq,
            unix_ms(),
            captured.tree,
            captured.blind,
        );
        let bytes = encode(&open)?;
        state.bytes = captured.usage.saturating_add(file_bytes(&bytes));
        self.store.write_state(&state)?;
        self.store
            .write_atomic(&self.store.open_path(&open.ticket)?, &bytes)?;
        Ok(open.ticket)
    }

    /// Captures the scope after the call and commits what changed, if anything did.
    pub(crate) fn finish(
        &self,
        ticket: &str,
        token: &CancellationToken,
    ) -> Result<Option<RecordSummary>, SnapshotError> {
        let workspace = self.workspace()?;
        let open = self.store.open_record(ticket)?;
        let mut state = self.store.state()?;
        let captured = capture(
            &self.store,
            workspace,
            &open.request.scope,
            Widening::Decided(open.widened),
            &open.request.limits,
            state.bytes,
            token,
        )?;
        state.bytes = captured.usage;
        let changes = self
            .store
            .objects()
            .changes(&open.before()?, &captured.tree)
            .map_err(store_error)?
            .changes;
        let window = self.window(open.opened_seq, state.next_seq)?;
        let (changes, mut unrecorded) = rebase(changes, &window);
        unrecorded.extend(blind_changes(&open.blind, &captured.blind));
        unrecorded.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        unrecorded.dedup_by(|left, right| left.path == right.path);
        let record = (!changes.is_empty() || !unrecorded.is_empty())
            .then(|| StoredRecord::new(state.next_seq, &open, changes, unrecorded));
        if let Some(record) = &record {
            let bytes = encode(record)?;
            state.next_seq += 1;
            state.records += 1;
            state.bytes = state.bytes.saturating_add(file_bytes(&bytes));
            self.store.write_state(&state)?;
            self.store
                .write_atomic(&self.store.record_path(record.seq), &bytes)?;
        } else {
            self.store.write_state(&state)?;
        }
        if self.store.remove(&self.store.open_path(ticket)?)? {
            self.store.sync(OPEN)?;
        }
        let Some(record) = record else {
            return Ok(None);
        };
        if let Err(error) = self.retain(&mut state, open.request.limits.max_total_bytes, record.seq)
        {
            tracing::warn!(%error, seq = record.seq, "workspace change retention did not run");
        }
        Ok(Some(record.summary()))
    }

    /// Deletes an open record. Its objects stay for collection.
    pub(crate) fn abandon(&self, ticket: &str) -> Result<bool, SnapshotError> {
        let path = match self.store.open_path(ticket) {
            Err(SnapshotError::NotFound) => return Ok(false),
            path => path?,
        };
        let removed = self.store.remove(&path)?;
        if removed {
            self.store.sync(OPEN)?;
        }
        Ok(removed)
    }

    pub(crate) fn open_records_of(&self, holder: &str) -> Result<Vec<OpenRecord>, SnapshotError> {
        self.store
            .open_records()?
            .into_iter()
            .filter(|open| open.request.holder.as_str() == holder)
            .map(|open| {
                Ok(OpenRecord {
                    ticket: identifier(&open.ticket)?,
                    client: open.request.client,
                    opened_at_unix_ms: open.opened_at_unix_ms,
                })
            })
            .collect()
    }

    pub(crate) fn abandon_open_records_of(&self, holder: &str) -> Result<u32, SnapshotError> {
        let mut abandoned = 0_u32;
        for open in self.store.open_records()? {
            if open.request.holder.as_str() == holder
                && self.store.remove(&self.store.open_path(&open.ticket)?)?
            {
                abandoned = abandoned.saturating_add(1);
            }
        }
        if abandoned > 0 {
            self.store.sync(OPEN)?;
        }
        Ok(abandoned)
    }

    /// The records committed from `from` on, while an open record waited to finish.
    fn window(&self, from: u64, until: u64) -> Result<Vec<StoredRecord>, SnapshotError> {
        let mut window = Vec::new();
        for seq in from..until {
            window.extend(self.store.record(seq)?);
        }
        Ok(window)
    }
}

/// Rebases what one call changed onto `window`, the records committed while it ran, in seq order.
/// A path the window also changed keeps only what those records do not already hold: a change
/// both saw belongs to the earlier record, and a later change starts where the window left the
/// path. A path whose history the call cannot tell apart from the window's is unrecorded.
pub(crate) fn rebase(
    changes: Vec<Change>,
    window: &[StoredRecord],
) -> (Vec<StoredChange>, Vec<StoredUnrecorded>) {
    let mut edges = HashMap::<&str, Vec<(State, State)>>::new();
    let mut blind = HashSet::new();
    for record in window {
        for change in &record.changes {
            edges.entry(change.path.as_str()).or_default().push((
                change.before.map(|content| content.0),
                change.after.map(|content| content.0),
            ));
        }
        blind.extend(
            record
                .unrecorded
                .iter()
                .map(|unrecorded| unrecorded.path.as_str()),
        );
    }
    let mut kept = Vec::new();
    let mut interleaved = Vec::new();
    for change in changes {
        let edge = if blind.contains(change.path.as_str()) {
            Err(())
        } else {
            match edges.get(change.path.as_str()) {
                None => Ok(Some(change.source)),
                Some(chain) => chained(chain, change.source, change.target),
            }
        };
        match edge {
            Ok(Some(before)) => kept.push(StoredChange {
                path: change.path,
                before: before.map(StoredContent),
                after: change.target.map(StoredContent),
            }),
            Ok(None) => {}
            Err(()) => interleaved.push(StoredUnrecorded {
                path: change.path,
                reason: UnrecordedReason::Interleaved,
            }),
        }
    }
    (kept, interleaved)
}

/// Where a change from `before` to `after` starts once `chain` is accounted for: `None` when the
/// chain already ends there, and an error when the chain breaks or never passes `before`.
fn chained(chain: &[(State, State)], before: State, after: State) -> Result<Option<State>, ()> {
    if !chain.windows(2).all(|pair| pair[0].1 == pair[1].0) {
        return Err(());
    }
    let (first, last) = (chain[0].0, chain[chain.len() - 1].1);
    let on_chain = first == before || chain.iter().any(|(_, state)| *state == before);
    match (on_chain, after == last) {
        (false, _) => Err(()),
        (true, true) => Ok(None),
        (true, false) => Ok(Some(last)),
    }
}

/// The paths either capture saw but could not store. One both saw with the same stamp was left
/// alone; any other may have changed in a way no record holds.
pub(crate) fn blind_changes(before: &[Blind], after: &[Blind]) -> Vec<StoredUnrecorded> {
    let mut unrecorded = BTreeMap::new();
    let after_by_path = after
        .iter()
        .map(|blind| (blind.path.as_str(), blind))
        .collect::<HashMap<_, _>>();
    for blind in before {
        match after_by_path.get(blind.path.as_str()) {
            Some(later) if later.stamp.is_some() && later.stamp == blind.stamp => {}
            Some(later) => {
                unrecorded.insert(later.path.clone(), later.reason);
            }
            None => {
                unrecorded.insert(blind.path.clone(), blind.reason);
            }
        }
    }
    let before_paths = before
        .iter()
        .map(|blind| blind.path.as_str())
        .collect::<HashSet<_>>();
    for blind in after {
        if !before_paths.contains(blind.path.as_str()) {
            unrecorded.insert(blind.path.clone(), blind.reason);
        }
    }
    unrecorded
        .into_iter()
        .map(|(path, reason)| StoredUnrecorded { path, reason })
        .collect()
}

fn file_bytes(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use test_case::test_case;
    use workcell_snapshot_store::{EntryKind, blob_id};

    use super::*;
    use crate::format::Stamp;

    const PATH: &str = "file.txt";
    const OTHER_PATH: &str = "other.txt";

    fn content(text: &str) -> State {
        Some(Content {
            kind: EntryKind::File,
            oid: blob_id(text.as_bytes()).unwrap(),
        })
    }

    fn change(path: &str, source: State, target: State) -> Change {
        Change {
            path: path.to_owned(),
            source,
            target,
        }
    }

    fn record(edges: &[(&str, State, State)], unrecorded: &[&str]) -> StoredRecord {
        let mut record: StoredRecord = serde_json::from_value(serde_json::json!({
            "version": "workspace-change-record.v1",
            "seq": 1,
            "ticket": "rec_0123456789abcdef0123456789abcdef",
            "holders": ["holder"],
            "client": {},
            "changes": [],
            "unrecorded": [],
        }))
        .unwrap();
        record.changes = edges
            .iter()
            .map(|(path, before, after)| StoredChange {
                path: (*path).to_owned(),
                before: before.map(StoredContent),
                after: after.map(StoredContent),
            })
            .collect();
        record.unrecorded = unrecorded
            .iter()
            .map(|path| StoredUnrecorded {
                path: (*path).to_owned(),
                reason: UnrecordedReason::Oversized,
            })
            .collect();
        record
    }

    fn edges(changes: &[StoredChange]) -> Vec<(&str, State, State)> {
        changes
            .iter()
            .map(|change| {
                (
                    change.path.as_str(),
                    change.before.map(|content| content.0),
                    change.after.map(|content| content.0),
                )
            })
            .collect()
    }

    #[test]
    fn a_change_no_other_record_touched_is_kept_as_seen() {
        let (kept, unrecorded) = rebase(
            vec![change(PATH, content("a"), content("b"))],
            &[record(&[(OTHER_PATH, content("x"), content("y"))], &[])],
        );

        assert_eq!(edges(&kept), [(PATH, content("a"), content("b"))]);
        assert!(unrecorded.is_empty());
    }

    #[test_case(content("a"), content("b") ; "the same change")]
    #[test_case(None, content("b") ; "the same creation")]
    fn a_change_the_window_already_holds_belongs_to_the_earlier_record(
        before: State,
        after: State,
    ) {
        let (kept, unrecorded) = rebase(
            vec![change(PATH, before, after)],
            &[record(&[(PATH, before, after)], &[])],
        );

        assert!(kept.is_empty());
        assert!(unrecorded.is_empty());
    }

    #[test_case(content("a") ; "from where the window started")]
    #[test_case(content("b") ; "from where the window went")]
    fn a_later_change_starts_where_the_window_left_the_path(seen_before: State) {
        let window = [
            record(&[(PATH, content("a"), content("b"))], &[]),
            record(&[(PATH, content("b"), content("c"))], &[]),
        ];

        let (kept, unrecorded) = rebase(vec![change(PATH, seen_before, content("d"))], &window);

        assert_eq!(edges(&kept), [(PATH, content("c"), content("d"))]);
        assert!(unrecorded.is_empty());
    }

    #[test]
    fn a_change_from_a_state_the_window_never_passed_is_interleaved() {
        let (kept, unrecorded) = rebase(
            vec![change(PATH, content("x"), content("d"))],
            &[record(&[(PATH, content("a"), content("b"))], &[])],
        );

        assert!(kept.is_empty());
        assert_eq!(unrecorded.len(), 1);
        assert_eq!(unrecorded[0].reason, UnrecordedReason::Interleaved);
    }

    #[test]
    fn a_window_that_does_not_chain_leaves_the_path_interleaved() {
        let window = [
            record(&[(PATH, content("a"), content("b"))], &[]),
            record(&[(PATH, content("x"), content("c"))], &[]),
        ];

        let (kept, unrecorded) = rebase(vec![change(PATH, content("a"), content("d"))], &window);

        assert!(kept.is_empty());
        assert_eq!(unrecorded[0].reason, UnrecordedReason::Interleaved);
    }

    #[test]
    fn a_path_the_window_could_not_record_is_interleaved() {
        let (kept, unrecorded) = rebase(
            vec![change(PATH, content("a"), content("b"))],
            &[record(&[], &[PATH])],
        );

        assert!(kept.is_empty());
        assert_eq!(unrecorded[0].reason, UnrecordedReason::Interleaved);
    }

    fn blind(path: &str, reason: UnrecordedReason, stamp: Option<Stamp>) -> Blind {
        Blind {
            path: path.to_owned(),
            reason,
            stamp,
        }
    }

    fn stamp(path: &Path) -> Stamp {
        Stamp::of(&fs::metadata(path).unwrap())
    }

    #[test]
    fn a_blind_path_with_an_unchanged_stamp_was_left_alone() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let unchanged = [blind(
            PATH,
            UnrecordedReason::Oversized,
            Some(stamp(file.path())),
        )];

        assert!(blind_changes(&unchanged, &unchanged).is_empty());
    }

    #[test_case(
        &[blind(PATH, UnrecordedReason::Oversized, None)], &[] , UnrecordedReason::Oversized ;
        "blind before only"
    )]
    #[test_case(
        &[], &[blind(PATH, UnrecordedReason::Unstable, None)], UnrecordedReason::Unstable ;
        "blind after only"
    )]
    #[test_case(
        &[blind(PATH, UnrecordedReason::Blocked, None)],
        &[blind(PATH, UnrecordedReason::Blocked, None)],
        UnrecordedReason::Blocked ;
        "blind without a stamp"
    )]
    fn a_blind_path_that_may_have_changed_is_unrecorded(
        before: &[Blind],
        after: &[Blind],
        reason: UnrecordedReason,
    ) {
        let unrecorded = blind_changes(before, after);

        assert_eq!(unrecorded.len(), 1);
        assert_eq!(
            (unrecorded[0].path.as_str(), unrecorded[0].reason),
            (PATH, reason)
        );
    }
}
