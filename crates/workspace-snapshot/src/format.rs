//! What the store keeps besides objects: its state, open and finished records, revert journals and
//! eviction marks. Every file names its format, and decoding refuses any other.

use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    fs::Metadata,
    path::PathBuf,
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{DeserializeOwned, Error as _, IgnoredAny, SeqAccess, Visitor},
};
use workcell_host_contract::{
    RecordClientMetadata, RecordHolder, RecordRequest, RecordSummary, RevertDirection, RevertState,
    SnapshotLimit, UnrecordedReason,
};
use workcell_snapshot_store::{Content, EntryKind, ObjectId, SnapshotId, parse_oid};

use crate::{
    SnapshotError, hex_sha256, limit_error,
    snapshot::valid_path,
    store::{EVICTED, MAX_METADATA_BYTES, METADATA_SUFFIX, OPEN, RECORDS, REVERTS, Store},
};

pub(crate) const STATE_VERSION: &str = "workspace-changes-state.v1";
const OPEN_VERSION: &str = "workspace-change-open.v1";
const RECORD_VERSION: &str = "workspace-change-record.v1";
pub(crate) const JOURNAL_VERSION: &str = "workspace-revert-journal.v4";
const EVICTED_VERSION: &str = "workspace-change-evicted.v1";
const TICKET_PREFIX: &str = "rec_";
const REVERT_ID_PREFIX: &str = "revert_";
/// A v4 UUID without separators.
const RANDOM_ID_BYTES: usize = 32;
const SEQ_DIGITS: usize = 20;
const FILE_MODE: &str = "100644";
const EXECUTABLE_MODE: &str = "100755";
const SYMLINK_MODE: &str = "120000";
/// Seqs start here, so zero never names a record.
const FIRST_SEQ: u64 = 1;

/// A store file: its format version is checked on every read.
pub(crate) trait Stored: DeserializeOwned {
    const VERSION: &'static str;

    fn version(&self) -> &str;

    fn valid(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredState {
    version: String,
    pub(crate) next_seq: u64,
    /// An upper bound on what the store occupies, exact after every measurement.
    pub(crate) bytes: u64,
    /// Record files, tombstones included, exact after every measurement.
    pub(crate) records: u64,
}

/// Identity, size, mode and both timestamps of a file: equal stamps mean it was left alone.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Stamp(u64, u64, u64, u32, i64, i64, i64, i64);

/// A path a capture saw but could not store.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Blind {
    pub(crate) path: String,
    pub(crate) reason: UnrecordedReason,
    /// Present when the capture could tell whether a later one sees the same file.
    pub(crate) stamp: Option<Stamp>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredOpenRecord {
    version: String,
    pub(crate) ticket: String,
    pub(crate) request: RecordRequest,
    /// A named path was one the call can write through to another path, so both captures hold
    /// the whole tree as well.
    pub(crate) widened: bool,
    /// The seq the next committed record got when this one opened: every record from it on was
    /// committed while this call ran.
    pub(crate) opened_seq: u64,
    pub(crate) opened_at_unix_ms: u64,
    /// The snapshot tree of the scope before the call.
    pub(crate) before: String,
    pub(crate) blind: Vec<Blind>,
}

/// Git's form of an entry, `<mode> <oid>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StoredContent(pub(crate) Content);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredChange {
    pub(crate) path: String,
    pub(crate) before: Option<StoredContent>,
    pub(crate) after: Option<StoredContent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredUnrecorded {
    pub(crate) path: String,
    pub(crate) reason: UnrecordedReason,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRecord {
    version: String,
    pub(crate) seq: u64,
    pub(crate) ticket: String,
    /// Empty for a tombstone: released by every holder while an open record's window still
    /// needs its changes.
    pub(crate) holders: BTreeSet<String>,
    pub(crate) client: RecordClientMetadata,
    /// In path byte order.
    pub(crate) changes: Vec<StoredChange>,
    pub(crate) unrecorded: Vec<StoredUnrecorded>,
}

/// What a listing needs of a record, read without holding its changes.
#[derive(Deserialize)]
pub(crate) struct StoredRecordHeader {
    version: String,
    pub(crate) seq: u64,
    pub(crate) holders: BTreeSet<String>,
    pub(crate) client: RecordClientMetadata,
    pub(crate) changes: Length,
    pub(crate) unrecorded: Length,
}

/// The length of a sequence, counted without keeping its elements.
pub(crate) struct Length(pub(crate) usize);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredJournal {
    version: String,
    pub(crate) revert_id: String,
    pub(crate) holder: String,
    /// Position in the holder's stack of pending reverts.
    pub(crate) order: u64,
    pub(crate) direction: RevertDirection,
    pub(crate) seqs: Vec<u64>,
    pub(crate) state: RevertState,
    pub(crate) total_files: usize,
    pub(crate) applied_files: usize,
    pub(crate) total_directories: usize,
    pub(crate) created_directories: usize,
    pub(crate) reconciliation_required: bool,
    /// The path whose publication refused or could not be settled, which ended the revert.
    pub(crate) stopped_at: Option<String>,
}

/// The newest record retention evicted from one holder.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredEvicted {
    version: String,
    pub(crate) holder: String,
    pub(crate) seq: u64,
    pub(crate) client: RecordClientMetadata,
}

/// Only what tells one format from another.
#[derive(Deserialize)]
pub(crate) struct StoredVersion {
    pub(crate) version: String,
}

impl Stored for StoredState {
    const VERSION: &'static str = STATE_VERSION;

    fn version(&self) -> &str {
        &self.version
    }

    fn valid(&self) -> bool {
        self.next_seq >= FIRST_SEQ
    }
}

impl Stored for StoredOpenRecord {
    const VERSION: &'static str = OPEN_VERSION;

    fn version(&self) -> &str {
        &self.version
    }

    fn valid(&self) -> bool {
        valid_random_id(&self.ticket, TICKET_PREFIX)
            && self.before.parse::<SnapshotId>().is_ok()
            && self.blind.iter().all(|blind| valid_path(&blind.path))
    }
}

impl Stored for StoredRecord {
    const VERSION: &'static str = RECORD_VERSION;

    fn version(&self) -> &str {
        &self.version
    }

    fn valid(&self) -> bool {
        self.seq >= FIRST_SEQ
            && valid_random_id(&self.ticket, TICKET_PREFIX)
            && self.holders.iter().all(|holder| valid_holder(holder))
            && self
                .changes
                .iter()
                .all(|change| valid_path(&change.path) && change.before != change.after)
            && self
                .changes
                .windows(2)
                .all(|pair| pair[0].path.as_bytes() < pair[1].path.as_bytes())
            && self
                .unrecorded
                .iter()
                .all(|unrecorded| valid_path(&unrecorded.path))
    }
}

impl Stored for StoredRecordHeader {
    const VERSION: &'static str = RECORD_VERSION;

    fn version(&self) -> &str {
        &self.version
    }

    fn valid(&self) -> bool {
        self.seq >= FIRST_SEQ && self.holders.iter().all(|holder| valid_holder(holder))
    }
}

impl Stored for StoredJournal {
    const VERSION: &'static str = JOURNAL_VERSION;

    fn version(&self) -> &str {
        &self.version
    }

    fn valid(&self) -> bool {
        valid_random_id(&self.revert_id, REVERT_ID_PREFIX)
            && valid_holder(&self.holder)
            && !self.seqs.is_empty()
            && self.seqs.windows(2).all(|pair| pair[0] < pair[1])
            && self.applied_files <= self.total_files
            && self.created_directories <= self.total_directories
            && self.stopped_at.as_deref().is_none_or(valid_path)
    }
}

impl Stored for StoredEvicted {
    const VERSION: &'static str = EVICTED_VERSION;

    fn version(&self) -> &str {
        &self.version
    }

    fn valid(&self) -> bool {
        valid_holder(&self.holder) && self.seq >= FIRST_SEQ
    }
}

impl StoredState {
    pub(crate) fn new() -> Self {
        Self {
            version: STATE_VERSION.to_owned(),
            next_seq: FIRST_SEQ,
            bytes: 0,
            records: 0,
        }
    }
}

impl StoredOpenRecord {
    pub(crate) fn new(
        request: RecordRequest,
        widened: bool,
        opened_seq: u64,
        opened_at_unix_ms: u64,
        before: SnapshotId,
        blind: Vec<Blind>,
    ) -> Self {
        Self {
            version: OPEN_VERSION.to_owned(),
            ticket: random_id(TICKET_PREFIX),
            request,
            widened,
            opened_seq,
            opened_at_unix_ms,
            before: before.to_string(),
            blind,
        }
    }

    pub(crate) fn before(&self) -> Result<SnapshotId, SnapshotError> {
        self.before
            .parse()
            .map_err(|_| SnapshotError::UnhealthyStorage)
    }
}

impl StoredRecord {
    pub(crate) fn new(
        seq: u64,
        open: &StoredOpenRecord,
        changes: Vec<StoredChange>,
        unrecorded: Vec<StoredUnrecorded>,
    ) -> Self {
        Self {
            version: RECORD_VERSION.to_owned(),
            seq,
            ticket: open.ticket.clone(),
            holders: BTreeSet::from([open.request.holder.as_str().to_owned()]),
            client: open.request.client.clone(),
            changes,
            unrecorded,
        }
    }

    pub(crate) fn summary(&self) -> RecordSummary {
        RecordSummary {
            seq: self.seq,
            paths: count(self.changes.len()),
            unrecorded: count(self.unrecorded.len()),
        }
    }

    /// Every object the record names, each once.
    pub(crate) fn objects(&self) -> impl Iterator<Item = ObjectId> + '_ {
        let mut seen = HashSet::new();
        self.changes
            .iter()
            .flat_map(|change| [change.before, change.after])
            .flatten()
            .map(|content| content.0.oid)
            .filter(move |oid| seen.insert(*oid))
    }
}

impl<'de> Deserialize<'de> for Length {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Counter;

        impl<'de> Visitor<'de> for Counter {
            type Value = Length;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a sequence")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Length, A::Error> {
                let mut length = 0;
                while sequence.next_element::<IgnoredAny>()?.is_some() {
                    length += 1;
                }
                Ok(Length(length))
            }
        }

        deserializer.deserialize_seq(Counter)
    }
}

impl StoredJournal {
    pub(crate) fn new(
        revert_id: String,
        holder: &str,
        order: u64,
        direction: RevertDirection,
        seqs: Vec<u64>,
        total_files: usize,
        total_directories: usize,
    ) -> Self {
        Self {
            version: JOURNAL_VERSION.to_owned(),
            revert_id,
            holder: holder.to_owned(),
            order,
            direction,
            seqs,
            state: RevertState::Publishing,
            total_files,
            applied_files: 0,
            total_directories,
            created_directories: 0,
            reconciliation_required: false,
            stopped_at: None,
        }
    }
}

impl StoredEvicted {
    pub(crate) fn new(holder: &str, seq: u64, client: RecordClientMetadata) -> Self {
        Self {
            version: EVICTED_VERSION.to_owned(),
            holder: holder.to_owned(),
            seq,
            client,
        }
    }
}

impl Stamp {
    #[cfg(unix)]
    pub(crate) fn of(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self(
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mode(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    }

    #[cfg(not(unix))]
    pub(crate) fn of(metadata: &Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or((0, 0), |duration| {
                (
                    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                    i64::from(duration.subsec_nanos()),
                )
            });
        Self(
            0,
            0,
            metadata.len(),
            0,
            modified.0,
            modified.1,
            modified.0,
            modified.1,
        )
    }
}

impl Serialize for StoredContent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mode = match self.0.kind {
            EntryKind::File => FILE_MODE,
            EntryKind::Executable => EXECUTABLE_MODE,
            EntryKind::Symlink => SYMLINK_MODE,
        };
        serializer.collect_str(&format_args!("{mode} {}", self.0.oid))
    }
}

impl<'de> Deserialize<'de> for StoredContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let invalid = || D::Error::custom("invalid stored entry");
        let (mode, oid) = text.split_once(' ').ok_or_else(invalid)?;
        let kind = match mode {
            FILE_MODE => EntryKind::File,
            EXECUTABLE_MODE => EntryKind::Executable,
            SYMLINK_MODE => EntryKind::Symlink,
            _ => return Err(invalid()),
        };
        Ok(Self(Content {
            kind,
            oid: parse_oid(oid).ok_or_else(invalid)?,
        }))
    }
}

/// A store file's bytes, refused when too large to be read back.
pub(crate) fn encode(value: &(impl Stored + Serialize)) -> Result<Vec<u8>, SnapshotError> {
    let bytes = serde_json::to_vec(value).map_err(|_| SnapshotError::OperationFailed)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_METADATA_BYTES {
        return Err(limit_error(
            SnapshotLimit::MetadataBytes,
            MAX_METADATA_BYTES,
        ));
    }
    Ok(bytes)
}

pub(crate) fn decode<T: Stored>(bytes: &[u8]) -> Result<T, SnapshotError> {
    let value: T = serde_json::from_slice(bytes).map_err(|_| SnapshotError::UnhealthyStorage)?;
    if value.version() != T::VERSION || !value.valid() {
        return Err(SnapshotError::UnhealthyStorage);
    }
    Ok(value)
}

/// Typed access to the store's files. Callers hold the store lock.
impl Store {
    pub(crate) fn state(&self) -> Result<StoredState, SnapshotError> {
        decode(&self.read(&self.state_path())?)
    }

    pub(crate) fn write_state(&self, state: &StoredState) -> Result<(), SnapshotError> {
        self.write_atomic(&self.state_path(), &encode(state)?)
    }

    pub(crate) fn open_path(&self, ticket: &str) -> Result<PathBuf, SnapshotError> {
        if !valid_random_id(ticket, TICKET_PREFIX) {
            return Err(SnapshotError::NotFound);
        }
        Ok(self.file(OPEN, &format!("{ticket}{METADATA_SUFFIX}")))
    }

    pub(crate) fn open_record(&self, ticket: &str) -> Result<StoredOpenRecord, SnapshotError> {
        let open: StoredOpenRecord = decode(&self.read(&self.open_path(ticket)?)?)?;
        if open.ticket != ticket {
            return Err(SnapshotError::UnhealthyStorage);
        }
        Ok(open)
    }

    /// Every open record. One that cannot be read fails the listing, so nothing a record names
    /// is ever mistaken for garbage.
    pub(crate) fn open_records(&self) -> Result<Vec<StoredOpenRecord>, SnapshotError> {
        self.names(OPEN)?
            .iter()
            .map(|name| {
                let ticket = name
                    .strip_suffix(METADATA_SUFFIX)
                    .ok_or(SnapshotError::UnhealthyStorage)?;
                match self.open_record(ticket) {
                    Err(SnapshotError::NotFound) => Err(SnapshotError::UnhealthyStorage),
                    open => open,
                }
            })
            .collect()
    }

    pub(crate) fn record_path(&self, seq: u64) -> PathBuf {
        self.file(RECORDS, &format!("{seq:0SEQ_DIGITS$}{METADATA_SUFFIX}"))
    }

    /// Every record's seq, in order.
    pub(crate) fn seqs(&self) -> Result<Vec<u64>, SnapshotError> {
        self.names(RECORDS)?
            .iter()
            .map(|name| {
                name.strip_suffix(METADATA_SUFFIX)
                    .filter(|digits| {
                        digits.len() == SEQ_DIGITS
                            && digits.bytes().all(|byte| byte.is_ascii_digit())
                    })
                    .and_then(|digits| digits.parse().ok())
                    .ok_or(SnapshotError::UnhealthyStorage)
            })
            .collect()
    }

    pub(crate) fn record(&self, seq: u64) -> Result<Option<StoredRecord>, SnapshotError> {
        let bytes = match self.read(&self.record_path(seq)) {
            Err(SnapshotError::NotFound) => return Ok(None),
            bytes => bytes?,
        };
        let record: StoredRecord = decode(&bytes)?;
        if record.seq != seq {
            return Err(SnapshotError::UnhealthyStorage);
        }
        Ok(Some(record))
    }

    /// Every record in seq order, failing on any that cannot be read.
    pub(crate) fn records(&self) -> Result<Vec<StoredRecord>, SnapshotError> {
        self.seqs()?
            .into_iter()
            .map(|seq| self.record(seq)?.ok_or(SnapshotError::UnhealthyStorage))
            .collect()
    }

    /// A record's header; a record deleted since its seq was listed is none.
    pub(crate) fn header(&self, seq: u64) -> Result<Option<StoredRecordHeader>, SnapshotError> {
        let bytes = match self.read(&self.record_path(seq)) {
            Err(SnapshotError::NotFound) => return Ok(None),
            bytes => bytes?,
        };
        let header: StoredRecordHeader = decode(&bytes)?;
        if header.seq != seq {
            return Err(SnapshotError::UnhealthyStorage);
        }
        Ok(Some(header))
    }

    /// Every record's header in seq order, skipping any deleted while listing.
    pub(crate) fn headers(&self) -> Result<Vec<StoredRecordHeader>, SnapshotError> {
        let mut headers = Vec::new();
        for seq in self.seqs()? {
            headers.extend(self.header(seq)?);
        }
        Ok(headers)
    }

    pub(crate) fn write_record(&self, record: &StoredRecord) -> Result<(), SnapshotError> {
        self.write_atomic(&self.record_path(record.seq), &encode(record)?)
    }

    pub(crate) fn journal_path(&self, revert_id: &str) -> Result<PathBuf, SnapshotError> {
        if !valid_random_id(revert_id, REVERT_ID_PREFIX) {
            return Err(SnapshotError::NotFound);
        }
        Ok(self.file(REVERTS, &format!("{revert_id}{METADATA_SUFFIX}")))
    }

    /// Every revert journal, in stack order within each holder, skipping any settled while
    /// listing.
    pub(crate) fn journals(&self) -> Result<Vec<StoredJournal>, SnapshotError> {
        let mut journals = Vec::new();
        for name in self.names(REVERTS)? {
            let revert_id = name
                .strip_suffix(METADATA_SUFFIX)
                .ok_or(SnapshotError::UnhealthyStorage)?;
            let bytes = match self.read(&self.journal_path(revert_id)?) {
                Err(SnapshotError::NotFound) => continue,
                bytes => bytes?,
            };
            let journal: StoredJournal = decode(&bytes)?;
            if journal.revert_id != revert_id {
                return Err(SnapshotError::UnhealthyStorage);
            }
            journals.push(journal);
        }
        journals.sort_unstable_by(|left, right| {
            (&left.holder, left.order).cmp(&(&right.holder, right.order))
        });
        Ok(journals)
    }

    pub(crate) fn write_journal(&self, journal: &StoredJournal) -> Result<(), SnapshotError> {
        self.write_atomic(&self.journal_path(&journal.revert_id)?, &encode(journal)?)
    }

    pub(crate) fn evicted_path(&self, holder: &str) -> PathBuf {
        self.file(
            EVICTED,
            &format!("{}{METADATA_SUFFIX}", hex_sha256(holder.as_bytes())),
        )
    }

    pub(crate) fn evicted(&self, holder: &str) -> Result<Option<StoredEvicted>, SnapshotError> {
        let bytes = match self.read(&self.evicted_path(holder)) {
            Err(SnapshotError::NotFound) => return Ok(None),
            bytes => bytes?,
        };
        let evicted: StoredEvicted = decode(&bytes)?;
        if evicted.holder != holder {
            return Err(SnapshotError::UnhealthyStorage);
        }
        Ok(Some(evicted))
    }

    /// Notes that retention evicted `seq` from `holder`, unless a newer eviction already did.
    pub(crate) fn mark_evicted(
        &self,
        holder: &str,
        seq: u64,
        client: &RecordClientMetadata,
    ) -> Result<(), SnapshotError> {
        if self
            .evicted(holder)?
            .is_some_and(|evicted| evicted.seq >= seq)
        {
            return Ok(());
        }
        self.write_atomic(
            &self.evicted_path(holder),
            &encode(&StoredEvicted::new(holder, seq, client.clone()))?,
        )
    }
}

pub(crate) fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

pub(crate) fn holder(value: &str) -> Result<RecordHolder, SnapshotError> {
    RecordHolder::new(value).map_err(|_| SnapshotError::UnhealthyStorage)
}

fn valid_holder(holder: &str) -> bool {
    RecordHolder::new(holder).is_ok()
}

pub(crate) fn revert_id() -> String {
    random_id(REVERT_ID_PREFIX)
}

fn random_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

fn valid_random_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|random| {
        random.len() == RANDOM_ID_BYTES
            && random
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use workcell_snapshot_store::blob_id;

    use super::*;

    const CONTENT: &[u8] = b"content";

    #[test_case(EntryKind::File, FILE_MODE ; "a plain file")]
    #[test_case(EntryKind::Executable, EXECUTABLE_MODE ; "an executable file")]
    #[test_case(EntryKind::Symlink, SYMLINK_MODE ; "a link")]
    fn stored_content_uses_git_modes_and_round_trips(kind: EntryKind, mode: &str) {
        let oid = blob_id(CONTENT).unwrap();
        let content = StoredContent(Content { kind, oid });
        let wire = serde_json::to_value(content).unwrap();

        assert_eq!(wire, serde_json::json!(format!("{mode} {oid}")));
        assert_eq!(
            serde_json::from_value::<StoredContent>(wire).unwrap(),
            content
        );
    }

    #[test_case("100600 0123" ; "an unknown mode")]
    #[test_case("100644 zz" ; "a malformed object id")]
    #[test_case("100644" ; "a missing object id")]
    fn stored_content_refuses_malformed_entries(text: &str) {
        assert!(serde_json::from_value::<StoredContent>(serde_json::json!(text)).is_err());
    }

    #[test_case("rec_0123456789abcdef0123456789abcdef", TICKET_PREFIX, true ; "a generated ticket")]
    #[test_case("rec_0123456789ABCDEF0123456789ABCDEF", TICKET_PREFIX, false ; "uppercase hex")]
    #[test_case("rec_../../../etc/passwd", TICKET_PREFIX, false ; "a path traversal")]
    #[test_case("revert_0123456789abcdef0123456789abcdef", TICKET_PREFIX, false ; "another prefix")]
    fn random_ids_name_only_what_the_store_generates(id: &str, prefix: &str, valid: bool) {
        assert_eq!(valid_random_id(id, prefix), valid);
    }

    #[test]
    fn a_state_of_another_version_is_refused() {
        let mut state = serde_json::to_value(StoredState::new()).unwrap();
        state["version"] = serde_json::json!("workspace-changes-state.v2");

        assert_eq!(
            decode::<StoredState>(&serde_json::to_vec(&state).unwrap()).unwrap_err(),
            SnapshotError::UnhealthyStorage
        );
    }
}
