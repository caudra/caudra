//! A project's memory notes as an append-only journal, with a binary tree of
//! one-line summaries built over it in the background.
//!
//! The notes stay Markdown files on disk; the journal records every version
//! each of them had. A scope is one project's journal. Its entries are
//! numbered 0, 1, 2, ... without gaps, so a summary node is named by its level
//! and index alone: node `(level, index)` covers the entries from
//! `index << level` up to, not including, `(index + 1) << level`.

use std::collections::HashMap;
use std::io;

use rusqlite::{
    Connection, OptionalExtension, Params, Row, Transaction, TransactionBehavior, params,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::sessions::{SessionDatabase, SessionError};
use crate::{StateClass, StateDir};

pub const MAX_ENTRY_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_NODE_TEXT_BYTES: usize = 4 * 1024;
pub const MAX_HEADING_BYTES: usize = 120;
const NOTE_KIND: &str = "note";
const DELETE_KIND: &str = "delete";
const SESSION_ORIGIN_PREFIX: &str = "session:";
const EXTERNAL_ORIGIN: &str = "external";
const IMPORT_ORIGIN: &str = "import";
const FRONTMATTER_FENCE: &str = "---";
const HEADING_MARKER: char = '#';
const KIND_FIELD: &str = "entry kind";
const ORIGIN_FIELD: &str = "entry origin";
const SEQ_FIELD: &str = "sequence number";
const INDEX_FIELD: &str = "node index";
const BODY_BYTES_FIELD: &str = "body size";
/// Every [`EntryMeta`] column, in the order [`entry_meta`] reads them. The
/// body is the table's last column and `octet_length` takes its size from the
/// record header, so listing entries never loads a body.
const META_COLUMNS: &str =
    "seq, kind, name, heading, octet_length(body), origin, created_ms, forgotten";
/// Holds for the row that is its name's live note: the newest entry of that
/// name, a note, and not forgotten.
const LIVE: &str = "kind = 'note' AND forgotten = 0 AND seq = (SELECT max(newer.seq) \
    FROM memory_entries newer WHERE newer.scope = memory_entries.scope \
    AND newer.name = memory_entries.name)";
const NEXT_SEQ: &str = "SELECT coalesce(max(seq) + 1, 0) FROM memory_entries WHERE scope = ?1";

/// The body comes last so every other column of a row sits ahead of its
/// overflow pages.
pub(crate) const TABLES: &str = r#"
CREATE TABLE memory_entries (
    scope      TEXT NOT NULL,
    seq        INTEGER NOT NULL CHECK(seq >= 0),
    kind       TEXT NOT NULL CHECK(kind IN ('note', 'delete')),
    name       TEXT NOT NULL,
    heading    TEXT NOT NULL,
    hash       BLOB CHECK(hash IS NULL OR length(hash) = 32),
    origin     TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    forgotten  INTEGER NOT NULL DEFAULT 0 CHECK(forgotten IN (0, 1)),
    body       TEXT NOT NULL,
    UNIQUE(scope, seq)
) STRICT;
CREATE INDEX memory_entries_name ON memory_entries(scope, name, seq);

CREATE TABLE memory_nodes (
    scope            TEXT NOT NULL,
    level            INTEGER NOT NULL CHECK(level >= 0),
    idx              INTEGER NOT NULL CHECK(idx >= 0),
    text             TEXT,
    model            TEXT,
    created_ms       INTEGER NOT NULL,
    lease_owner      TEXT,
    lease_expires_ms INTEGER,
    PRIMARY KEY(scope, level, idx)
) STRICT, WITHOUT ROWID;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Note,
    Delete,
}

impl EntryKind {
    const fn storage_name(self) -> &'static str {
        match self {
            Self::Note => NOTE_KIND,
            Self::Delete => DELETE_KIND,
        }
    }

    fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            NOTE_KIND => Some(Self::Note),
            DELETE_KIND => Some(Self::Delete),
            _ => None,
        }
    }
}

/// Who made a change: a session, an edit to the file made outside Caudra, or
/// an import.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EntryOrigin {
    Session(String),
    External,
    Import,
}

impl EntryOrigin {
    fn storage_name(&self) -> String {
        match self {
            Self::Session(id) => format!("{SESSION_ORIGIN_PREFIX}{id}"),
            Self::External => EXTERNAL_ORIGIN.to_owned(),
            Self::Import => IMPORT_ORIGIN.to_owned(),
        }
    }

    fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            EXTERNAL_ORIGIN => Some(Self::External),
            IMPORT_ORIGIN => Some(Self::Import),
            _ => value
                .strip_prefix(SESSION_ORIGIN_PREFIX)
                .map(|id| Self::Session(id.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMeta {
    pub seq: u64,
    pub kind: EntryKind,
    pub name: String,
    pub heading: String,
    pub body_bytes: u64,
    pub origin: EntryOrigin,
    pub created_ms: i64,
    pub forgotten: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub meta: EntryMeta,
    pub body: String,
}

/// A summary node. One without text is only a lease: an owner is building it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNode {
    pub level: u32,
    pub index: u64,
    pub text: Option<String>,
    pub model: Option<String>,
    pub created_ms: i64,
    pub lease_owner: Option<String>,
    pub lease_expires_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalChange {
    Note {
        name: String,
        body: String,
        created_ms: i64,
    },
    Delete {
        name: String,
        created_ms: i64,
    },
}

impl JournalChange {
    fn name(&self) -> &str {
        match self {
            Self::Note { name, .. } | Self::Delete { name, .. } => name,
        }
    }

    fn created_ms(&self) -> i64 {
        match self {
            Self::Note { created_ms, .. } | Self::Delete { created_ms, .. } => *created_ms,
        }
    }

    fn validate(&self) -> Result<(), MemoryJournalError> {
        match self {
            Self::Note { name, body, .. } if body.len() > MAX_ENTRY_BODY_BYTES => {
                Err(MemoryJournalError::TooLarge {
                    name: name.clone(),
                    bytes: body.len(),
                })
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ForgetReport {
    pub entries: usize,
    pub nodes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryJournalError {
    #[error("memory journal storage operation failed: {0}")]
    Session(#[from] SessionError),
    #[error("memory journal database operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("memory note change failed: {0}")]
    Io(#[from] io::Error),
    #[error("memory note {name} is {bytes} bytes, maximum is {maximum}", maximum = MAX_ENTRY_BODY_BYTES)]
    TooLarge { name: String, bytes: usize },
    #[error(
        "memory summary at level {level}, index {index} is {bytes} bytes, maximum is {maximum}",
        maximum = MAX_NODE_TEXT_BYTES
    )]
    NodeTooLarge {
        level: u32,
        index: u64,
        bytes: usize,
    },
    #[error("invalid memory journal {field}: {value}")]
    Invalid { field: &'static str, value: String },
}

pub fn body_hash(body: &str) -> [u8; 32] {
    Sha256::digest(body.as_bytes()).into()
}

/// The line a note is known by: its first line with text past any YAML
/// frontmatter, without heading markers, cut to [`MAX_HEADING_BYTES`]. A fence
/// that never closes is not frontmatter, or it would swallow the whole note.
pub fn heading(body: &str) -> String {
    let mut lines = body.lines().map(str::trim);
    let mut text_lines = lines.clone();
    if lines.find(|line| !line.is_empty()) == Some(FRONTMATTER_FENCE)
        && lines.any(|line| line == FRONTMATTER_FENCE)
    {
        text_lines = lines;
    }
    let text = text_lines
        .map(|line| line.trim_start_matches(HEADING_MARKER).trim_start())
        .find(|text| !text.is_empty())
        .unwrap_or_default();
    text[..text.floor_char_boundary(MAX_HEADING_BYTES)].to_owned()
}

/// Every scope's journal and summary tree. They live in the persistent root,
/// beside the notes they record.
pub struct MemoryJournal {
    database: SessionDatabase,
}

impl MemoryJournal {
    pub fn open(state_dir: &StateDir) -> Result<Self, MemoryJournalError> {
        Ok(Self {
            database: SessionDatabase::open_state(&state_dir.for_class(StateClass::Persistent))?,
        })
    }

    /// How many entries the scope ever had: one past its newest number.
    pub fn len(&self, scope: &str) -> Result<u64, MemoryJournalError> {
        self.connection()
            .query_row_and_then(NEXT_SEQ, [scope], |row| row_u64(row, 0, SEQ_FIELD))
    }

    /// Entries numbered `from_seq` on, oldest first, without their bodies.
    pub fn entries(
        &self,
        scope: &str,
        from_seq: u64,
    ) -> Result<Vec<EntryMeta>, MemoryJournalError> {
        self.query_rows(
            &format!(
                "SELECT {META_COLUMNS} FROM memory_entries WHERE scope = ?1 AND seq >= ?2 \
                 ORDER BY seq"
            ),
            params![scope, sql_integer(from_seq, SEQ_FIELD)?],
            entry_meta,
        )
    }

    pub fn entry(&self, scope: &str, seq: u64) -> Result<Option<Entry>, MemoryJournalError> {
        Ok(self.bodies(scope, &[seq])?.pop())
    }

    /// The entries `seqs` names, oldest first. A number naming no entry is
    /// skipped.
    pub fn bodies(&self, scope: &str, seqs: &[u64]) -> Result<Vec<Entry>, MemoryJournalError> {
        self.query_rows(
            &format!(
                "SELECT {META_COLUMNS}, body FROM memory_entries WHERE scope = ?1 \
                 AND seq IN (SELECT value FROM json_each(?2)) ORDER BY seq"
            ),
            params![scope, Value::from(seqs.to_vec()).to_string()],
            entry,
        )
    }

    /// Every live note: per name, its newest entry when that is a note nobody
    /// forgot. Oldest first.
    pub fn current(&self, scope: &str) -> Result<Vec<Entry>, MemoryJournalError> {
        self.query_rows(
            &format!(
                "SELECT {META_COLUMNS}, body FROM memory_entries WHERE scope = ?1 AND {LIVE} \
                 ORDER BY seq"
            ),
            [scope],
            entry,
        )
    }

    /// The live note named `name`, if there is one.
    pub fn live(&self, scope: &str, name: &str) -> Result<Option<Entry>, MemoryJournalError> {
        Ok(self
            .query_rows::<_, Vec<_>>(
                &format!(
                    "SELECT {META_COLUMNS}, body FROM memory_entries \
                     WHERE scope = ?1 AND name = ?2 AND {LIVE}"
                ),
                params![scope, name],
                entry,
            )?
            .pop())
    }

    /// Runs `read` in one read transaction, so a caller combining several
    /// queries sees the journal and the tree as they stood at one moment.
    pub fn read<T>(
        &self,
        read: impl FnOnce(&Self) -> Result<T, MemoryJournalError>,
    ) -> Result<T, MemoryJournalError> {
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Deferred)?;
        let value = read(self)?;
        transaction.commit()?;
        Ok(value)
    }

    pub fn current_hashes(
        &self,
        scope: &str,
    ) -> Result<HashMap<String, [u8; 32]>, MemoryJournalError> {
        self.query_rows(
            &format!("SELECT name, hash FROM memory_entries WHERE scope = ?1 AND {LIVE}"),
            [scope],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    /// Every node row, built or only leased, by level then index.
    pub fn nodes(&self, scope: &str) -> Result<Vec<StoredNode>, MemoryJournalError> {
        self.query_rows(
            "SELECT level, idx, text, model, created_ms, lease_owner, lease_expires_ms \
             FROM memory_nodes WHERE scope = ?1 ORDER BY level, idx",
            [scope],
            |row| {
                Ok(StoredNode {
                    level: row.get(0)?,
                    index: row_u64(row, 1, INDEX_FIELD)?,
                    text: row.get(2)?,
                    model: row.get(3)?,
                    created_ms: row.get(4)?,
                    lease_owner: row.get(5)?,
                    lease_expires_ms: row.get(6)?,
                })
            },
        )
    }

    /// Runs `effect`, which writes or deletes the note's file, while holding
    /// the journal's write lock, so racing processes leave the files and the
    /// journal in the same order. Then records `change` and returns its number,
    /// unless it changes nothing: a note whose live version has the same body,
    /// or a delete of a name with no live note.
    pub fn append(
        &self,
        scope: &str,
        change: JournalChange,
        origin: &EntryOrigin,
        effect: impl FnOnce() -> io::Result<()>,
    ) -> Result<Option<u64>, MemoryJournalError> {
        change.validate()?;
        let transaction = self.begin_write()?;
        effect()?;
        let seq = record(&transaction, scope, &change, &origin.storage_name())?;
        transaction.commit()?;
        Ok(seq)
    }

    /// Records changes found on disk under the rules [`Self::append`] skips
    /// by, so the same changes reconciled again, through this journal or
    /// another, append nothing.
    pub fn reconcile(
        &self,
        scope: &str,
        changes: &[JournalChange],
        origin: &EntryOrigin,
    ) -> Result<Vec<u64>, MemoryJournalError> {
        changes.iter().try_for_each(JournalChange::validate)?;
        let origin = origin.storage_name();
        let transaction = self.begin_write()?;
        let mut appended = Vec::new();
        for change in changes {
            appended.extend(record(&transaction, scope, change, &origin)?);
        }
        transaction.commit()?;
        Ok(appended)
    }

    /// Leases node `(level, index)` to `owner` until `now_ms + lease_ms`. A
    /// node with no row is free; an unbuilt one is free once its lease expires,
    /// and its holder may renew it before then.
    pub fn claim(
        &self,
        scope: &str,
        level: u32,
        index: u64,
        owner: &str,
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<bool, MemoryJournalError> {
        let claimed = self.connection().execute(
            "INSERT INTO memory_nodes (scope, level, idx, created_ms, lease_owner, lease_expires_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(scope, level, idx) DO UPDATE SET created_ms = excluded.created_ms, \
             lease_owner = excluded.lease_owner, lease_expires_ms = excluded.lease_expires_ms \
             WHERE text IS NULL AND (lease_owner = excluded.lease_owner \
             OR lease_expires_ms <= excluded.created_ms)",
            params![
                scope,
                level,
                sql_integer(index, INDEX_FIELD)?,
                now_ms,
                owner,
                now_ms.saturating_add(lease_ms)
            ],
        )?;
        Ok(claimed > 0)
    }

    /// Builds the node `owner` holds the lease on, expired or not. False when
    /// the lease is gone: forgetting a note removed it, or another owner took
    /// it over.
    #[allow(clippy::too_many_arguments)]
    pub fn store_node(
        &self,
        scope: &str,
        level: u32,
        index: u64,
        owner: &str,
        text: &str,
        model: &str,
        now_ms: i64,
    ) -> Result<bool, MemoryJournalError> {
        if text.len() > MAX_NODE_TEXT_BYTES {
            return Err(MemoryJournalError::NodeTooLarge {
                level,
                index,
                bytes: text.len(),
            });
        }
        let stored = self.connection().execute(
            "UPDATE memory_nodes SET text = ?5, model = ?6, created_ms = ?7, lease_owner = NULL, \
             lease_expires_ms = NULL \
             WHERE scope = ?1 AND level = ?2 AND idx = ?3 AND text IS NULL AND lease_owner = ?4",
            params![
                scope,
                level,
                sql_integer(index, INDEX_FIELD)?,
                owner,
                text,
                model,
                now_ms
            ],
        )?;
        Ok(stored > 0)
    }

    /// Gives up the lease `owner` holds on an unbuilt node.
    pub fn release(
        &self,
        scope: &str,
        level: u32,
        index: u64,
        owner: &str,
    ) -> Result<(), MemoryJournalError> {
        self.connection().execute(
            "DELETE FROM memory_nodes \
             WHERE scope = ?1 AND level = ?2 AND idx = ?3 AND text IS NULL AND lease_owner = ?4",
            params![scope, level, sql_integer(index, INDEX_FIELD)?, owner],
        )?;
        Ok(())
    }

    /// Runs `effect`, which deletes the note's file, while holding the write
    /// lock. Then blanks every entry of `name` and drops every node, built or
    /// leased, whose range ends after the name's first entry, because any
    /// summary written since could have read the note. A node ends after
    /// `first` exactly when `idx >= first >> level`, which, unlike
    /// `(idx + 1) << level`, cannot overflow.
    pub fn forget(
        &self,
        scope: &str,
        name: &str,
        effect: impl FnOnce() -> io::Result<()>,
    ) -> Result<ForgetReport, MemoryJournalError> {
        let transaction = self.begin_write()?;
        effect()?;
        let nodes = transaction.execute(
            "DELETE FROM memory_nodes WHERE scope = ?1 AND idx >= \
             ((SELECT min(seq) FROM memory_entries WHERE scope = ?1 AND name = ?2) >> level)",
            params![scope, name],
        )?;
        let entries = transaction.execute(
            "UPDATE memory_entries SET body = '', heading = '', hash = NULL, forgotten = 1 \
             WHERE scope = ?1 AND name = ?2 AND forgotten = 0",
            params![scope, name],
        )?;
        transaction.commit()?;
        Ok(ForgetReport { entries, nodes })
    }

    /// Deletes the scope's whole journal and tree. Returns how many entries
    /// went.
    pub fn purge(&self, scope: &str) -> Result<u64, MemoryJournalError> {
        let transaction = self.begin_write()?;
        let entries =
            transaction.execute("DELETE FROM memory_entries WHERE scope = ?1", [scope])?;
        transaction.execute("DELETE FROM memory_nodes WHERE scope = ?1", [scope])?;
        transaction.commit()?;
        Ok(entries as u64)
    }

    fn connection(&self) -> &Connection {
        self.database.connection()
    }

    fn begin_write(&self) -> Result<Transaction<'_>, MemoryJournalError> {
        Ok(Transaction::new_unchecked(
            self.connection(),
            TransactionBehavior::Immediate,
        )?)
    }

    fn query_rows<T, C: FromIterator<T>>(
        &self,
        sql: &str,
        params: impl Params,
        read: impl FnMut(&Row<'_>) -> Result<T, MemoryJournalError>,
    ) -> Result<C, MemoryJournalError> {
        self.connection()
            .prepare(sql)?
            .query_and_then(params, read)?
            .collect()
    }
}

/// Appends `change` unless it changes nothing, and returns its number.
fn record(
    transaction: &Transaction<'_>,
    scope: &str,
    change: &JournalChange,
    origin: &str,
) -> Result<Option<u64>, MemoryJournalError> {
    let live: Option<[u8; 32]> = transaction
        .query_row(
            &format!("SELECT hash FROM memory_entries WHERE scope = ?1 AND name = ?2 AND {LIVE}"),
            params![scope, change.name()],
            |row| row.get(0),
        )
        .optional()?;
    let (kind, body, hash) = match change {
        JournalChange::Note { body, .. } => {
            let hash = body_hash(body);
            if live == Some(hash) {
                return Ok(None);
            }
            (EntryKind::Note, body.as_str(), Some(hash))
        }
        JournalChange::Delete { .. } if live.is_none() => return Ok(None),
        JournalChange::Delete { .. } => (EntryKind::Delete, "", None),
    };
    let seq = transaction.query_row_and_then(
        &format!(
            "INSERT INTO memory_entries \
             (scope, seq, kind, name, heading, hash, origin, created_ms, body) \
             VALUES (?1, ({NEXT_SEQ}), ?2, ?3, ?4, ?5, ?6, ?7, ?8) RETURNING seq"
        ),
        params![
            scope,
            kind.storage_name(),
            change.name(),
            heading(body),
            hash,
            origin,
            change.created_ms(),
            body
        ],
        |row| row_u64(row, 0, SEQ_FIELD),
    )?;
    Ok(Some(seq))
}

/// Reads a row selected with [`META_COLUMNS`].
fn entry_meta(row: &Row<'_>) -> Result<EntryMeta, MemoryJournalError> {
    let kind: String = row.get(1)?;
    let origin: String = row.get(5)?;
    Ok(EntryMeta {
        seq: row_u64(row, 0, SEQ_FIELD)?,
        kind: EntryKind::from_storage_name(&kind).ok_or_else(|| MemoryJournalError::Invalid {
            field: KIND_FIELD,
            value: kind,
        })?,
        name: row.get(2)?,
        heading: row.get(3)?,
        body_bytes: row_u64(row, 4, BODY_BYTES_FIELD)?,
        origin: EntryOrigin::from_storage_name(&origin).ok_or_else(|| {
            MemoryJournalError::Invalid {
                field: ORIGIN_FIELD,
                value: origin,
            }
        })?,
        created_ms: row.get(6)?,
        forgotten: row.get(7)?,
    })
}

/// Reads a row selected with [`META_COLUMNS`] followed by the body.
fn entry(row: &Row<'_>) -> Result<Entry, MemoryJournalError> {
    Ok(Entry {
        meta: entry_meta(row)?,
        body: row.get(8)?,
    })
}

fn sql_integer(value: u64, field: &'static str) -> Result<i64, MemoryJournalError> {
    i64::try_from(value).map_err(|_| MemoryJournalError::Invalid {
        field,
        value: value.to_string(),
    })
}

fn row_u64(row: &Row<'_>, column: usize, field: &'static str) -> Result<u64, MemoryJournalError> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|_| MemoryJournalError::Invalid {
        field,
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::thread;

    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    use super::{
        Entry, EntryKind, EntryMeta, EntryOrigin, ForgetReport, JournalChange,
        MAX_ENTRY_BODY_BYTES, MAX_HEADING_BYTES, MAX_NODE_TEXT_BYTES, MemoryJournal,
        MemoryJournalError, StoredNode, body_hash, heading,
    };
    use crate::StateDir;

    const SCOPE: &str = "project-a";
    const OTHER_SCOPE: &str = "project-b";
    const SESSION: &str = "session-a";
    const NAME: &str = "build.md";
    const OTHER_NAME: &str = "style.md";
    const NEW_NAME: &str = "release.md";
    const MISSING_NAME: &str = "missing.md";
    const BODY: &str = "# Build\n\nRun make check before make test.";
    const HEADING: &str = "Build";
    const EDITED_BODY: &str = "# Build\n\nRun make lint as well.";
    const OTHER_BODY: &str = "Keep naïve and résumé spelled with accents.";
    const OWNER: &str = "owner-a";
    const OTHER_OWNER: &str = "owner-b";
    const SUMMARY: &str = "How to build and which spellings to keep.";
    const MODEL: &str = "fast-model";
    const EFFECT_ERROR: &str = "disk full";
    const CREATED_MS: i64 = 1_000;
    const NOW_MS: i64 = 2_000;
    const LEASE_MS: i64 = 500;
    const EXPIRY_MS: i64 = NOW_MS + LEASE_MS;
    const MISSING_SEQ: u64 = 99;
    const WRITERS: u64 = 4;
    const NOTES_PER_WRITER: u64 = 8;

    fn fixture() -> (TempDir, StateDir, MemoryJournal) {
        let root = tempdir().unwrap();
        let state = StateDir::from_path(root.path().to_path_buf());
        let journal = MemoryJournal::open(&state).unwrap();
        (root, state, journal)
    }

    fn origin() -> EntryOrigin {
        EntryOrigin::Session(SESSION.to_owned())
    }

    fn note(name: &str, body: &str) -> JournalChange {
        JournalChange::Note {
            name: name.to_owned(),
            body: body.to_owned(),
            created_ms: CREATED_MS,
        }
    }

    fn delete(name: &str) -> JournalChange {
        JournalChange::Delete {
            name: name.to_owned(),
            created_ms: CREATED_MS,
        }
    }

    fn append(journal: &MemoryJournal, scope: &str, change: JournalChange) -> Option<u64> {
        journal.append(scope, change, &origin(), || Ok(())).unwrap()
    }

    fn build(journal: &MemoryJournal, scope: &str, level: u32, index: u64) {
        assert!(
            journal
                .claim(scope, level, index, OWNER, NOW_MS, LEASE_MS)
                .unwrap()
        );
        assert!(
            journal
                .store_node(scope, level, index, OWNER, SUMMARY, MODEL, NOW_MS)
                .unwrap()
        );
    }

    fn node_keys(journal: &MemoryJournal, scope: &str) -> Vec<(u32, u64)> {
        journal
            .nodes(scope)
            .unwrap()
            .iter()
            .map(|node| (node.level, node.index))
            .collect()
    }

    fn seqs_and_bodies(entries: Vec<Entry>) -> Vec<(u64, String)> {
        entries
            .into_iter()
            .map(|entry| (entry.meta.seq, entry.body))
            .collect()
    }

    fn failing_effect() -> io::Result<()> {
        Err(io::Error::other(EFFECT_ERROR))
    }

    #[test]
    fn append_numbers_each_scope_without_gaps_and_skips_changes_that_change_nothing() {
        let (_root, _state, journal) = fixture();
        let appended = [
            note(NAME, BODY),
            note(NAME, BODY),
            delete(MISSING_NAME),
            note(NAME, EDITED_BODY),
            delete(NAME),
            delete(NAME),
            note(NAME, BODY),
        ]
        .map(|change| append(&journal, SCOPE, change));
        assert_eq!(
            appended,
            [Some(0), None, None, Some(1), Some(2), None, Some(3)]
        );
        assert_eq!(journal.len(SCOPE).unwrap(), 4);
        assert_eq!(append(&journal, OTHER_SCOPE, note(NAME, BODY)), Some(0));
    }

    #[test]
    fn concurrent_writers_share_one_gapless_numbering() {
        let (_root, state, journal) = fixture();
        thread::scope(|threads| {
            for writer in 0..WRITERS {
                let state = &state;
                threads.spawn(move || {
                    let journal = MemoryJournal::open(state).unwrap();
                    for note_index in 0..NOTES_PER_WRITER {
                        append(
                            &journal,
                            SCOPE,
                            note(&format!("{writer}-{note_index}"), BODY),
                        );
                    }
                });
            }
        });
        let seqs: Vec<u64> = journal
            .entries(SCOPE, 0)
            .unwrap()
            .iter()
            .map(|entry| entry.seq)
            .collect();
        assert_eq!(seqs, (0..WRITERS * NOTES_PER_WRITER).collect::<Vec<_>>());
    }

    #[test]
    fn failed_effects_leave_journal_and_tree_untouched() {
        let (_root, _state, journal) = fixture();
        let appended = journal.append(SCOPE, note(NAME, BODY), &origin(), failing_effect);
        assert!(
            matches!(appended, Err(MemoryJournalError::Io(error)) if error.to_string() == EFFECT_ERROR)
        );
        assert_eq!(journal.len(SCOPE).unwrap(), 0);

        append(&journal, SCOPE, note(NAME, BODY));
        build(&journal, SCOPE, 0, 0);
        let forgotten = journal.forget(SCOPE, NAME, failing_effect);
        assert!(
            matches!(forgotten, Err(MemoryJournalError::Io(error)) if error.to_string() == EFFECT_ERROR)
        );
        assert_eq!(journal.entry(SCOPE, 0).unwrap().unwrap().body, BODY);
        assert_eq!(node_keys(&journal, SCOPE), [(0, 0)]);
    }

    #[test]
    fn oversized_notes_are_refused_before_any_effect_or_write() {
        let (_root, _state, journal) = fixture();
        let body = "x".repeat(MAX_ENTRY_BODY_BYTES + 1);
        let mut ran = false;
        let appended = journal.append(SCOPE, note(NAME, &body), &origin(), || {
            ran = true;
            Ok(())
        });
        assert!(
            matches!(appended, Err(MemoryJournalError::TooLarge { bytes, .. }) if bytes == body.len())
        );
        assert!(!ran);
        let reconciled = journal.reconcile(
            SCOPE,
            &[note(OTHER_NAME, BODY), note(NAME, &body)],
            &EntryOrigin::External,
        );
        assert!(matches!(
            reconciled,
            Err(MemoryJournalError::TooLarge { .. })
        ));
        assert_eq!(journal.len(SCOPE).unwrap(), 0);
        assert_eq!(append(&journal, SCOPE, note(NAME, &body[1..])), Some(0));
    }

    #[test]
    fn entries_read_metadata_alone_and_bodies_read_only_the_numbers_named() {
        let (_root, _state, journal) = fixture();
        append(&journal, SCOPE, note(NAME, BODY));
        journal
            .append(
                SCOPE,
                note(OTHER_NAME, OTHER_BODY),
                &EntryOrigin::External,
                || Ok(()),
            )
            .unwrap();
        append(&journal, SCOPE, delete(NAME));

        assert_eq!(
            journal.entries(SCOPE, 1).unwrap(),
            [
                EntryMeta {
                    seq: 1,
                    kind: EntryKind::Note,
                    name: OTHER_NAME.to_owned(),
                    heading: OTHER_BODY.to_owned(),
                    body_bytes: OTHER_BODY.len() as u64,
                    origin: EntryOrigin::External,
                    created_ms: CREATED_MS,
                    forgotten: false,
                },
                EntryMeta {
                    seq: 2,
                    kind: EntryKind::Delete,
                    name: NAME.to_owned(),
                    heading: String::new(),
                    body_bytes: 0,
                    origin: origin(),
                    created_ms: CREATED_MS,
                    forgotten: false,
                },
            ]
        );
        assert_eq!(
            seqs_and_bodies(journal.bodies(SCOPE, &[2, MISSING_SEQ, 0]).unwrap()),
            [(0, BODY.to_owned()), (2, String::new())]
        );
        assert_eq!(
            journal.entry(SCOPE, 0).unwrap().unwrap().meta.heading,
            HEADING
        );
        assert_eq!(journal.entry(SCOPE, MISSING_SEQ).unwrap(), None);
        assert_eq!(journal.entry(OTHER_SCOPE, 0).unwrap(), None);
    }

    #[test_case(EntryOrigin::Session(SESSION.to_owned()); "session")]
    #[test_case(EntryOrigin::External; "external")]
    #[test_case(EntryOrigin::Import; "import")]
    fn origin_survives_storage(origin: EntryOrigin) {
        let (_root, _state, journal) = fixture();
        journal
            .append(SCOPE, note(NAME, BODY), &origin, || Ok(()))
            .unwrap();
        assert_eq!(journal.entries(SCOPE, 0).unwrap()[0].origin, origin);
    }

    #[test]
    fn current_keeps_each_names_newest_live_note() {
        let (_root, _state, journal) = fixture();
        for change in [
            note(NAME, BODY),
            note(OTHER_NAME, OTHER_BODY),
            note(NAME, EDITED_BODY),
            delete(OTHER_NAME),
            note(NEW_NAME, BODY),
        ] {
            append(&journal, SCOPE, change);
        }
        assert_eq!(
            seqs_and_bodies(journal.current(SCOPE).unwrap()),
            [(2, EDITED_BODY.to_owned()), (4, BODY.to_owned())]
        );
        assert_eq!(
            journal.current_hashes(SCOPE).unwrap(),
            HashMap::from([
                (NAME.to_owned(), body_hash(EDITED_BODY)),
                (NEW_NAME.to_owned(), body_hash(BODY)),
            ])
        );
    }

    #[test]
    fn reconcile_appends_only_what_changed_through_any_journal() {
        let (_root, state, journal) = fixture();
        append(&journal, SCOPE, note(NAME, BODY));
        append(&journal, SCOPE, note(OTHER_NAME, OTHER_BODY));
        let changes = [
            note(NAME, BODY),
            delete(OTHER_NAME),
            delete(MISSING_NAME),
            note(NEW_NAME, EDITED_BODY),
        ];
        assert_eq!(
            journal
                .reconcile(SCOPE, &changes, &EntryOrigin::External)
                .unwrap(),
            [2, 3]
        );
        let other = MemoryJournal::open(&state).unwrap();
        assert!(
            other
                .reconcile(SCOPE, &changes, &EntryOrigin::External)
                .unwrap()
                .is_empty()
        );
        assert_eq!(journal.len(SCOPE).unwrap(), 4);
    }

    #[test]
    fn claim_leases_a_node_to_one_owner_until_it_expires_or_is_built() {
        let (_root, _state, journal) = fixture();
        assert!(journal.claim(SCOPE, 0, 0, OWNER, NOW_MS, LEASE_MS).unwrap());
        assert_eq!(
            journal.nodes(SCOPE).unwrap(),
            [StoredNode {
                level: 0,
                index: 0,
                text: None,
                model: None,
                created_ms: NOW_MS,
                lease_owner: Some(OWNER.to_owned()),
                lease_expires_ms: Some(EXPIRY_MS),
            }]
        );
        assert!(
            !journal
                .claim(SCOPE, 0, 0, OTHER_OWNER, EXPIRY_MS - 1, LEASE_MS)
                .unwrap()
        );
        assert!(
            journal
                .claim(SCOPE, 0, 0, OWNER, EXPIRY_MS - 1, LEASE_MS)
                .unwrap()
        );
        assert!(
            !journal
                .claim(SCOPE, 0, 0, OTHER_OWNER, EXPIRY_MS, LEASE_MS)
                .unwrap()
        );
        let renewed_expiry = EXPIRY_MS - 1 + LEASE_MS;
        assert!(
            journal
                .claim(SCOPE, 0, 0, OTHER_OWNER, renewed_expiry, LEASE_MS)
                .unwrap()
        );
        assert!(
            !journal
                .store_node(SCOPE, 0, 0, OWNER, SUMMARY, MODEL, renewed_expiry)
                .unwrap()
        );
        assert!(
            journal
                .store_node(SCOPE, 0, 0, OTHER_OWNER, SUMMARY, MODEL, renewed_expiry)
                .unwrap()
        );
        assert!(
            !journal
                .claim(SCOPE, 0, 0, OWNER, i64::MAX, LEASE_MS)
                .unwrap()
        );
        assert_eq!(
            journal.nodes(SCOPE).unwrap(),
            [StoredNode {
                level: 0,
                index: 0,
                text: Some(SUMMARY.to_owned()),
                model: Some(MODEL.to_owned()),
                created_ms: renewed_expiry,
                lease_owner: None,
                lease_expires_ms: None,
            }]
        );
    }

    #[test]
    fn holder_stores_its_expired_lease_and_release_frees_only_its_own_unbuilt_node() {
        let (_root, _state, journal) = fixture();
        journal.claim(SCOPE, 1, 2, OWNER, NOW_MS, LEASE_MS).unwrap();
        journal.release(SCOPE, 1, 2, OTHER_OWNER).unwrap();
        assert_eq!(node_keys(&journal, SCOPE), [(1, 2)]);
        journal.release(SCOPE, 1, 2, OWNER).unwrap();
        assert!(journal.nodes(SCOPE).unwrap().is_empty());

        journal.claim(SCOPE, 1, 2, OWNER, NOW_MS, LEASE_MS).unwrap();
        assert!(
            journal
                .store_node(SCOPE, 1, 2, OWNER, SUMMARY, MODEL, EXPIRY_MS)
                .unwrap()
        );
        journal.release(SCOPE, 1, 2, OWNER).unwrap();
        assert_eq!(node_keys(&journal, SCOPE), [(1, 2)]);
    }

    #[test]
    fn store_node_refuses_an_oversized_summary() {
        let (_root, _state, journal) = fixture();
        journal.claim(SCOPE, 0, 0, OWNER, NOW_MS, LEASE_MS).unwrap();
        let text = "x".repeat(MAX_NODE_TEXT_BYTES + 1);
        let stored = journal.store_node(SCOPE, 0, 0, OWNER, &text, MODEL, NOW_MS);
        assert!(
            matches!(stored, Err(MemoryJournalError::NodeTooLarge { bytes, .. }) if bytes == text.len())
        );
        assert!(
            journal
                .store_node(SCOPE, 0, 0, OWNER, &text[1..], MODEL, NOW_MS)
                .unwrap()
        );
    }

    #[test]
    fn forget_blanks_the_names_entries_and_drops_every_node_reaching_past_its_first_entry() {
        let (_root, _state, journal) = fixture();
        for change in [
            note(OTHER_NAME, OTHER_BODY),
            note(NEW_NAME, BODY),
            note(NAME, BODY),
            note(NAME, EDITED_BODY),
        ] {
            append(&journal, SCOPE, change);
        }
        for (level, index) in [(0, 0), (0, 1), (0, 2), (0, 3), (1, 0), (1, 1), (2, 0)] {
            build(&journal, SCOPE, level, index);
        }
        journal.claim(SCOPE, 0, 4, OWNER, NOW_MS, LEASE_MS).unwrap();
        build(&journal, OTHER_SCOPE, 0, 2);

        let mut ran = false;
        let report = journal
            .forget(SCOPE, NAME, || {
                ran = true;
                Ok(())
            })
            .unwrap();

        assert!(ran);
        assert_eq!(
            report,
            ForgetReport {
                entries: 2,
                nodes: 5
            }
        );
        assert_eq!(node_keys(&journal, SCOPE), [(0, 0), (0, 1), (1, 0)]);
        assert_eq!(node_keys(&journal, OTHER_SCOPE), [(0, 2)]);
        for entry in journal.bodies(SCOPE, &[2, 3]).unwrap() {
            assert_eq!(entry.body, "");
            assert_eq!(entry.meta.heading, "");
            assert_eq!(entry.meta.body_bytes, 0);
            assert!(entry.meta.forgotten);
        }
        let live: Vec<String> = journal
            .current(SCOPE)
            .unwrap()
            .into_iter()
            .map(|entry| entry.meta.name)
            .collect();
        assert_eq!(live, [OTHER_NAME, NEW_NAME]);
        assert_eq!(
            journal.forget(SCOPE, NAME, || Ok(())).unwrap(),
            ForgetReport::default()
        );
        assert_eq!(
            journal.forget(SCOPE, MISSING_NAME, || Ok(())).unwrap(),
            ForgetReport::default()
        );
    }

    #[test]
    fn purge_removes_only_that_scopes_journal_and_tree() {
        let (_root, _state, journal) = fixture();
        append(&journal, SCOPE, note(NAME, BODY));
        append(&journal, SCOPE, note(OTHER_NAME, OTHER_BODY));
        append(&journal, OTHER_SCOPE, note(NAME, BODY));
        build(&journal, SCOPE, 0, 0);
        build(&journal, OTHER_SCOPE, 0, 0);

        assert_eq!(journal.purge(SCOPE).unwrap(), 2);

        assert_eq!(journal.len(SCOPE).unwrap(), 0);
        assert!(journal.nodes(SCOPE).unwrap().is_empty());
        assert_eq!(journal.len(OTHER_SCOPE).unwrap(), 1);
        assert_eq!(node_keys(&journal, OTHER_SCOPE), [(0, 0)]);
    }

    #[test_case("# Build\n\nRun make.", "Build"; "markdown_heading")]
    #[test_case("\n\n  First line  \nsecond", "First line"; "first_line_with_text")]
    #[test_case("#\n##  \nTitle", "Title"; "bare_markers_skipped")]
    #[test_case("---\nname: build\n---\n\n## Build steps", "Build steps"; "frontmatter_skipped")]
    #[test_case("---\nname: build", "---"; "unclosed_fence_is_text")]
    #[test_case("", ""; "empty")]
    fn heading_is_the_first_line_with_text(body: &str, expected: &str) {
        assert_eq!(heading(body), expected);
    }

    #[test]
    fn heading_is_cut_at_a_character_boundary() {
        let body = format!("a{}", "é".repeat(MAX_HEADING_BYTES));
        assert_eq!(
            heading(&body),
            format!("a{}", "é".repeat((MAX_HEADING_BYTES - 1) / 2))
        );
    }
}
