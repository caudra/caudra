//! Schema 44: images leave the payloads for the media store, and the payloads
//! keep a reference. The same column list is what media cleanup scans for
//! references, so every column the step can write a reference into is a root.
//!
//! Only objects that are exactly an inline image move: `{"type":"base64",
//! "media_type","data"}` or the older `{"media_type","data"}`, with an image
//! media type. Everything else, and anything under a tool call's `input`, is
//! copied byte for byte, because the rewrite splices the raw text rather than
//! reserializing it.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use memchr::{memchr2, memmem};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, Row, Transaction, params, params_from_iter};
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, warn};

use super::{
    MIGRATION, MigrationEvent, SESSIONS_DB_FILE, SessionDatabase, SessionError, compress_payload,
    decompress_payload, from_i64, immutable_database_uri, to_i64,
};
use crate::backups::BACKUPS_DIR;
use crate::media::{
    MEDIA_GRACE, MediaCheckReport, MediaError, MediaHash, MediaHealth, MediaPruneReport,
    MediaStore, StoredImage, referenced_media,
};
use crate::sessions::{ARCHIVE_DIR, SESSIONS_DIR};
use crate::{StateDir, StorageError, atomic_write_permissions};

/// The first schema whose payloads hold media references.
pub(super) const MEDIA_SCHEMA: i64 = 44;
const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const INLINE_TYPE: &str = "base64";
/// What a payload holding an inline image must contain, as serde writes it.
const INLINE_DATA_NEEDLE: &[u8] = b"\"data\":\"";
const IMAGE_KEYS: [&[u8]; 3] = [b"\"type\"", b"\"media_type\"", b"\"data\""];
const TOOL_INPUT_KEY: &[u8] = b"\"input\"";
const MAX_SCAN_DEPTH: usize = 256;
const PROGRESS_INTERVAL: u64 = 512;
/// Rewritten payloads held before they are written. The scan pauses there and
/// resumes after the last key, so memory stays flat on a large database.
const PENDING_BYTES: usize = 64 * 1024 * 1024;
const BACKUP_SUFFIX: &str = ".bak";
const BACKUP_VERSION_PREFIX: &str = ".v";
const SIDECAR_SUFFIX: &str = ".media";
const SIDECAR_MODE: u32 = 0o600;
/// A backup this recent may still be being copied, so its references are
/// read but not cached.
const BACKUP_SETTLE: Duration = Duration::from_secs(60 * 60);

enum Encoding {
    Compressed,
    Text,
}

/// One column that holds JSON a stored image can sit in.
struct ImageColumn {
    table: &'static str,
    keys: &'static [&'static str],
    payload: &'static str,
    encoding: Encoding,
    /// The column holding the payload's uncompressed length, if any.
    length: Option<&'static str>,
    /// Whether that length counts toward `sessions.logical_bytes`, whose
    /// session id is then the first key.
    logical: bool,
}

const IMAGE_COLUMNS: [ImageColumn; 9] = [
    ImageColumn {
        table: "main_history_items",
        keys: &["session_id", "ordinal"],
        payload: "payload",
        encoding: Encoding::Compressed,
        length: Some("byte_count"),
        logical: true,
    },
    ImageColumn {
        table: "subagent_history_items",
        keys: &["session_id", "subagent_id", "ordinal"],
        payload: "payload",
        encoding: Encoding::Compressed,
        length: Some("byte_count"),
        logical: true,
    },
    ImageColumn {
        table: "tool_outputs",
        keys: &["session_id", "tool_id"],
        payload: "payload",
        encoding: Encoding::Compressed,
        length: Some("byte_count"),
        logical: true,
    },
    ImageColumn {
        table: "job_owner_checkpoints",
        keys: &["session_id", "invocation_id"],
        payload: "payload",
        encoding: Encoding::Compressed,
        length: Some("byte_count"),
        logical: false,
    },
    ImageColumn {
        table: "background_transcripts",
        keys: &["session_id", "invocation_id"],
        payload: "payload",
        encoding: Encoding::Compressed,
        length: Some("byte_count"),
        logical: false,
    },
    ImageColumn {
        table: "background_tasks",
        keys: &["session_id", "invocation_id"],
        payload: "payload",
        encoding: Encoding::Text,
        length: Some("bytes"),
        logical: false,
    },
    ImageColumn {
        table: "learning_transcripts",
        keys: &["run_id"],
        payload: "body",
        encoding: Encoding::Compressed,
        length: None,
        logical: false,
    },
    ImageColumn {
        table: "sessions",
        keys: &["id"],
        payload: "metadata",
        encoding: Encoding::Text,
        length: None,
        logical: true,
    },
    ImageColumn {
        table: "state",
        keys: &["scope", "key"],
        payload: "value",
        encoding: Encoding::Text,
        length: None,
        logical: false,
    },
];

impl ImageColumn {
    fn read(&self, row: &Row<'_>, index: usize) -> Result<String, SessionError> {
        Ok(match self.encoding {
            Encoding::Compressed => decompress_payload(&row.get::<_, Vec<u8>>(index)?, self.table)?,
            Encoding::Text => row.get(index)?,
        })
    }

    fn encode(&self, payload: String) -> Result<SqlValue, SessionError> {
        Ok(match self.encoding {
            Encoding::Compressed => SqlValue::Blob(compress_payload(&payload)?),
            Encoding::Text => SqlValue::Text(payload),
        })
    }

    fn select(&self, resume: bool) -> String {
        let Self {
            table,
            keys,
            payload,
            ..
        } = self;
        let keys = keys.join(", ");
        let filter = if resume {
            format!(" WHERE ({keys}) > ({})", placeholders(1, self.keys.len()))
        } else {
            String::new()
        };
        format!("SELECT {keys}, {payload} FROM {table}{filter} ORDER BY {keys}")
    }

    fn update(&self) -> String {
        let Self {
            table,
            keys,
            payload,
            length,
            ..
        } = self;
        let length = length.map_or_else(String::new, |length| format!(", {length} = ?2"));
        let first_key = if self.length.is_some() { 3 } else { 2 };
        let matches_key = keys
            .iter()
            .enumerate()
            .map(|(index, key)| format!("{key} = ?{}", first_key + index))
            .collect::<Vec<_>>()
            .join(" AND ");
        format!("UPDATE {table} SET {payload} = ?1{length} WHERE {matches_key}")
    }
}

fn placeholders(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

struct Pending {
    key: Vec<SqlValue>,
    stored: SqlValue,
    old_len: usize,
    new_len: usize,
}

/// The migration step: every inline image moves into the store and its
/// payload is rewritten in this transaction. A put always finishes before the
/// transaction commits, so a crash leaves files no payload names yet, which
/// cleanup reclaims, and rerunning the step converts what remains.
pub(super) fn store_inline_images(
    transaction: &Transaction<'_>,
    state_dir: &StateDir,
) -> Result<(), SessionError> {
    let store = MediaStore::new(state_dir);
    for column in &IMAGE_COLUMNS {
        rewrite_column(transaction, column, &store, PENDING_BYTES)?;
    }
    Ok(())
}

fn rewrite_column(
    transaction: &Transaction<'_>,
    column: &ImageColumn,
    store: &MediaStore,
    pending_limit: usize,
) -> Result<(), SessionError> {
    let table = column.table;
    let key_count = column.keys.len();
    let counted: i64 =
        transaction.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
    let total = from_i64(counted, "payload rows")?;
    let mut scanned: u64 = 0;
    let mut rewritten: u64 = 0;
    let mut images: u64 = 0;
    let mut bytes_before: u64 = 0;
    let mut bytes_after: u64 = 0;
    let mut update = transaction.prepare(&column.update())?;
    let mut resume: Option<Vec<SqlValue>> = None;
    MIGRATION.report(MigrationEvent::Rewrite {
        table,
        done: 0,
        total,
    });
    loop {
        let mut pending: Vec<Pending> = Vec::new();
        let mut pending_bytes = 0;
        {
            let mut select = transaction.prepare(&column.select(resume.is_some()))?;
            let mut rows = select.query(params_from_iter(resume.iter().flatten()))?;
            resume = None;
            while let Some(row) = rows.next()? {
                scanned += 1;
                if scanned.is_multiple_of(PROGRESS_INTERVAL) {
                    MIGRATION.report(MigrationEvent::Rewrite {
                        table,
                        done: scanned,
                        total,
                    });
                }
                let payload = match column.read(row, key_count) {
                    Ok(payload) => payload,
                    Err(error) => {
                        warn!(table, %error, "left the images of an unreadable row inline");
                        continue;
                    }
                };
                let Some(rewrite) = store_images(&payload, store, table)? else {
                    continue;
                };
                images += rewrite.images;
                bytes_before += payload.len() as u64;
                bytes_after += rewrite.payload.len() as u64;
                let new_len = rewrite.payload.len();
                let stored = column.encode(rewrite.payload)?;
                pending_bytes += match &stored {
                    SqlValue::Blob(bytes) => bytes.len(),
                    _ => new_len,
                };
                pending.push(Pending {
                    key: (0..key_count)
                        .map(|index| row.get::<_, SqlValue>(index))
                        .collect::<Result<_, _>>()?,
                    stored,
                    old_len: payload.len(),
                    new_len,
                });
                if pending_bytes >= pending_limit {
                    resume = pending.last().map(|last| last.key.clone());
                    break;
                }
            }
        }
        rewritten += pending.len() as u64;
        for Pending {
            key,
            stored,
            old_len,
            new_len,
        } in pending
        {
            let session = column.logical.then(|| key[0].clone());
            let mut values = vec![stored];
            if column.length.is_some() {
                values.push(SqlValue::Integer(to_i64(new_len, "payload bytes")?));
            }
            values.extend(key);
            update.execute(params_from_iter(values))?;
            if let Some(session) = session {
                let delta = to_i64(new_len, "payload bytes")? - to_i64(old_len, "payload bytes")?;
                transaction.execute(
                    "UPDATE sessions SET logical_bytes = logical_bytes + ?1 WHERE id = ?2",
                    params![delta, session],
                )?;
            }
        }
        if resume.is_none() {
            break;
        }
    }
    MIGRATION.report(MigrationEvent::Rewrite {
        table,
        done: scanned,
        total,
    });
    info!(
        table,
        scanned,
        rewritten,
        images,
        bytes_before,
        bytes_after,
        "moved inline images into the media store"
    );
    Ok(())
}

struct Rewrite {
    payload: String,
    images: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InlineImage<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    media_type: Cow<'a, str>,
    #[serde(borrow)]
    data: Cow<'a, str>,
}

impl InlineImage<'_> {
    fn is_image(&self) -> bool {
        self.kind.as_deref().is_none_or(|kind| kind == INLINE_TYPE)
            && IMAGE_MEDIA_TYPES.contains(&self.media_type.as_ref())
    }
}

/// `payload` with every inline image stored and replaced by its reference, or
/// `None` when it holds none. Undecodable images and unreadable payloads stay
/// as they are; a store that cannot write fails the step.
fn store_images(
    payload: &str,
    store: &MediaStore,
    table: &'static str,
) -> Result<Option<Rewrite>, SessionError> {
    if memmem::find(payload.as_bytes(), INLINE_DATA_NEEDLE).is_none() {
        return Ok(None);
    }
    let spans = match image_shaped_objects(payload) {
        Ok(spans) => spans,
        Err(error) => {
            warn!(table, %error, "left the images of an unreadable payload inline");
            return Ok(None);
        }
    };
    let mut rewritten = String::new();
    let mut copied = 0;
    let mut images = 0;
    for span in spans {
        let Ok(image) = serde_json::from_str::<InlineImage>(&payload[span.clone()]) else {
            continue;
        };
        if !image.is_image() {
            continue;
        }
        let bytes = match BASE64.decode(image.data.as_bytes()) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(table, media_type = %image.media_type, %error, "left an undecodable image inline");
                continue;
            }
        };
        let media = store.put(&bytes).map_err(StorageError::from)?;
        let reference = serde_json::to_string(&StoredImage::Media {
            media_type: image.media_type.into_owned(),
            media,
        })
        .map_err(StorageError::from)?;
        if rewritten.is_empty() {
            rewritten.reserve(payload.len());
        }
        rewritten.push_str(&payload[copied..span.start]);
        rewritten.push_str(&reference);
        copied = span.end;
        images += 1;
    }
    if images == 0 {
        return Ok(None);
    }
    rewritten.push_str(&payload[copied..]);
    Ok(Some(Rewrite {
        payload: rewritten,
        images,
    }))
}

#[derive(Debug, Error, PartialEq, Eq)]
enum ScanError {
    #[error("payload is not JSON at byte {0}")]
    Malformed(usize),
    #[error("payload nests deeper than {MAX_SCAN_DEPTH}")]
    TooDeep,
}

/// The byte ranges of every object outside a tool call's `input` whose two
/// or three members are all strings under image keys. Only these can be an
/// inline image; the caller parses each one to decide.
fn image_shaped_objects(payload: &str) -> Result<Vec<Range<usize>>, ScanError> {
    let mut scanner = Scanner {
        text: payload.as_bytes(),
        at: 0,
        spans: Vec::new(),
    };
    scanner.value(0, false)?;
    scanner.whitespace();
    if scanner.at != scanner.text.len() {
        return Err(ScanError::Malformed(scanner.at));
    }
    Ok(scanner.spans)
}

struct Scanner<'a> {
    text: &'a [u8],
    at: usize,
    spans: Vec<Range<usize>>,
}

impl Scanner<'_> {
    /// Scans one value, returning whether it was a string.
    fn value(&mut self, depth: usize, in_input: bool) -> Result<bool, ScanError> {
        if depth > MAX_SCAN_DEPTH {
            return Err(ScanError::TooDeep);
        }
        self.whitespace();
        match self.peek()? {
            b'{' => self.object(depth, in_input).map(|()| false),
            b'[' => self.array(depth, in_input).map(|()| false),
            b'"' => self.string().map(|_| true),
            _ => self.scalar().map(|()| false),
        }
    }

    fn object(&mut self, depth: usize, in_input: bool) -> Result<(), ScanError> {
        let start = self.at;
        self.at += 1;
        self.whitespace();
        if self.peek()? == b'}' {
            self.at += 1;
            return Ok(());
        }
        let mut members = 0;
        let mut image_shaped = !in_input;
        loop {
            self.whitespace();
            let key = self.string()?;
            let key = &self.text[key];
            self.whitespace();
            self.expect(b':')?;
            let string = self.value(depth + 1, in_input || key == TOOL_INPUT_KEY)?;
            members += 1;
            image_shaped &= string && IMAGE_KEYS.contains(&key);
            self.whitespace();
            match self.next()? {
                b',' => {}
                b'}' => break,
                _ => return Err(ScanError::Malformed(self.at - 1)),
            }
        }
        if image_shaped && (2..=IMAGE_KEYS.len()).contains(&members) {
            self.spans.push(start..self.at);
        }
        Ok(())
    }

    fn array(&mut self, depth: usize, in_input: bool) -> Result<(), ScanError> {
        self.at += 1;
        self.whitespace();
        if self.peek()? == b']' {
            self.at += 1;
            return Ok(());
        }
        loop {
            self.value(depth + 1, in_input)?;
            self.whitespace();
            match self.next()? {
                b',' => {}
                b']' => return Ok(()),
                _ => return Err(ScanError::Malformed(self.at - 1)),
            }
        }
    }

    fn string(&mut self) -> Result<Range<usize>, ScanError> {
        let start = self.at;
        self.expect(b'"')?;
        loop {
            let found = memchr2(b'"', b'\\', &self.text[self.at..])
                .ok_or(ScanError::Malformed(self.text.len()))?;
            self.at += found;
            if self.text[self.at] == b'\\' {
                if self.at + 1 >= self.text.len() {
                    return Err(ScanError::Malformed(self.at));
                }
                self.at += 2;
                continue;
            }
            self.at += 1;
            return Ok(start..self.at);
        }
    }

    fn scalar(&mut self) -> Result<(), ScanError> {
        let start = self.at;
        while self.at < self.text.len()
            && !matches!(
                self.text[self.at],
                b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t'
            )
        {
            self.at += 1;
        }
        if self.at == start {
            return Err(ScanError::Malformed(start));
        }
        Ok(())
    }

    fn whitespace(&mut self) {
        while self
            .text
            .get(self.at)
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.at += 1;
        }
    }

    fn peek(&self) -> Result<u8, ScanError> {
        self.text
            .get(self.at)
            .copied()
            .ok_or(ScanError::Malformed(self.at))
    }

    fn next(&mut self) -> Result<u8, ScanError> {
        let byte = self.peek()?;
        self.at += 1;
        Ok(byte)
    }

    fn expect(&mut self, expected: u8) -> Result<(), ScanError> {
        if self.next()? == expected {
            Ok(())
        } else {
            Err(ScanError::Malformed(self.at - 1))
        }
    }
}

/// Why media cleanup deleted nothing, named by the root that failed.
#[derive(Debug, Error)]
pub enum MediaCleanupError {
    #[error("scanning the session database for media references failed: {0}")]
    Database(Box<SessionError>),
    #[error("scanning archive {path} for media references failed: {source}")]
    Archive { path: PathBuf, source: io::Error },
    #[error("scanning backup {path} for media references failed: {source}")]
    Backup {
        path: PathBuf,
        source: Box<SessionError>,
    },
    #[error("media cleanup failed: {0}")]
    Store(#[from] MediaError),
}

impl MediaCleanupError {
    pub fn root(&self) -> &'static str {
        match self {
            Self::Database(_) => "database",
            Self::Archive { .. } => "archive",
            Self::Backup { .. } => "backup",
            Self::Store(_) => "store",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MediaCleanupReport {
    pub unreferenced: MediaPruneReport,
    /// Files that stay only because a database backup refers to them.
    /// Caudra never deletes backups, so these stay until the user does.
    pub backup_only: MediaPruneReport,
    /// `<backup>.media` reference caches whose backup is gone.
    pub orphan_sidecars: usize,
}

impl SessionDatabase {
    /// Deletes media files nothing references once they are past
    /// [`MEDIA_GRACE`], and the reference caches of deleted backups, or
    /// counts them in a dry run. The roots are every payload of every
    /// session, the global state rows (the prompt stash among them), session
    /// archives, and database backups. Any root that cannot be read fails the
    /// whole cleanup closed. The caller holds the session artifact lock,
    /// which a migration storing media also holds.
    pub(crate) fn prune_unreferenced_media(
        &self,
        dry_run: bool,
    ) -> Result<MediaCleanupReport, MediaCleanupError> {
        let state_dir = self.state_dir();
        let orphan_sidecars = remove_orphan_sidecars(state_dir, dry_run)?;
        let mut referenced = HashSet::new();
        database_references(self.connection(), &mut referenced)
            .map_err(|error| MediaCleanupError::Database(Box::new(error)))?;
        archive_references(state_dir, &mut referenced)?;
        let mut backed_up = HashSet::new();
        backup_references(state_dir, &mut backed_up)?;
        let store = MediaStore::new(state_dir);
        let backup_only = store.usage_of(backed_up.difference(&referenced));
        referenced.extend(backed_up);
        let cutoff = SystemTime::now() - MEDIA_GRACE;
        let unreferenced = if dry_run {
            store.unreferenced(&referenced, cutoff)?
        } else {
            store.prune(&referenced, cutoff)?
        };
        Ok(MediaCleanupReport {
            unreferenced,
            backup_only,
            orphan_sidecars,
        })
    }

    /// Reads back every media file the database's payloads name. Archives
    /// and backups are not checked.
    pub fn check_media(&self) -> Result<MediaCheckReport, SessionError> {
        let mut referenced = HashSet::new();
        database_references(self.connection(), &mut referenced)?;
        let mut hashes: Vec<MediaHash> = referenced.into_iter().collect();
        hashes.sort_unstable();
        let store = MediaStore::new(self.state_dir());
        let mut report = MediaCheckReport {
            referenced: hashes.len(),
            ..MediaCheckReport::default()
        };
        for hash in hashes {
            match store.verify(&hash).map_err(StorageError::Io)? {
                MediaHealth::Intact => {}
                MediaHealth::Missing => report.missing.push(hash),
                MediaHealth::Corrupt => report.corrupt.push(hash),
            }
        }
        Ok(report)
    }
}

/// Deletes each `<backup>.media` whose backup is gone, or counts them in a
/// dry run.
fn remove_orphan_sidecars(state_dir: &StateDir, dry_run: bool) -> Result<usize, MediaCleanupError> {
    let root = state_dir.path().join(BACKUPS_DIR);
    let failed = |path: &Path| {
        let path = path.to_path_buf();
        move |error: io::Error| MediaCleanupError::Backup {
            path,
            source: Box::new(StorageError::Io(error).into()),
        }
    };
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(failed(&root)(error)),
    };
    let mut orphans = 0;
    for entry in entries {
        let entry = entry.map_err(failed(&root))?;
        let name = entry.file_name();
        let Some(backup) = name
            .to_str()
            .filter(|name| name.starts_with(SESSIONS_DB_FILE))
            .and_then(|name| name.strip_suffix(SIDECAR_SUFFIX))
            .filter(|backup| backup.ends_with(BACKUP_SUFFIX))
        else {
            continue;
        };
        let path = entry.path();
        if root.join(backup).try_exists().map_err(failed(&path))? {
            continue;
        }
        if !dry_run {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(failed(&path)(error)),
            }
        }
        orphans += 1;
    }
    Ok(orphans)
}

/// Every hash the payload columns of `connection` name, read in one
/// transaction so a reference moving between rows cannot be missed.
fn database_references(
    connection: &Connection,
    into: &mut HashSet<MediaHash>,
) -> Result<(), SessionError> {
    let transaction = connection.unchecked_transaction()?;
    for column in &IMAGE_COLUMNS {
        let mut select =
            transaction.prepare(&format!("SELECT {} FROM {}", column.payload, column.table))?;
        let mut rows = select.query([])?;
        while let Some(row) = rows.next()? {
            referenced_media(&column.read(row, 0)?, into);
        }
    }
    transaction.commit()?;
    Ok(())
}

fn archive_references(
    state_dir: &StateDir,
    into: &mut HashSet<MediaHash>,
) -> Result<(), MediaCleanupError> {
    let root = state_dir.path().join(SESSIONS_DIR).join(ARCHIVE_DIR);
    let failed = |path: &Path| {
        let path = path.to_path_buf();
        move |source| MediaCleanupError::Archive { path, source }
    };
    let sessions = match fs::read_dir(&root) {
        Ok(sessions) => sessions,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed(&root)(error)),
    };
    for session in sessions {
        let session = session.map_err(failed(&root))?;
        if !session.file_type().map_err(failed(&root))?.is_dir() {
            continue;
        }
        let directory = session.path();
        for file in fs::read_dir(&directory).map_err(failed(&directory))? {
            let file = file.map_err(failed(&directory))?;
            let path = file.path();
            if file.file_type().map_err(failed(&path))?.is_file() {
                text_references(&path, into).map_err(failed(&path))?;
            }
        }
    }
    Ok(())
}

fn text_references(path: &Path, into: &mut HashSet<MediaHash>) -> io::Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        referenced_media(&String::from_utf8_lossy(&line), into);
        line.clear();
    }
    Ok(())
}

/// The references of every database backup from schema 44 on. A backup never
/// changes, so its references are read once and kept beside it in
/// `<backup>.media`; older backups hold their images inline.
fn backup_references(
    state_dir: &StateDir,
    into: &mut HashSet<MediaHash>,
) -> Result<(), MediaCleanupError> {
    let root = state_dir.path().join(BACKUPS_DIR);
    let failed = |path: &Path| {
        let path = path.to_path_buf();
        move |source: SessionError| MediaCleanupError::Backup {
            path,
            source: Box::new(source),
        }
    };
    let io_failed = |path: &Path| {
        let failed = failed(path);
        move |error: io::Error| failed(StorageError::Io(error).into())
    };
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_failed(&root)(error)),
    };
    for entry in entries {
        let entry = entry.map_err(io_failed(&root))?;
        let name = entry.file_name();
        let Some(label) = name
            .to_str()
            .and_then(|name| name.strip_prefix(SESSIONS_DB_FILE))
            .and_then(|name| name.strip_suffix(BACKUP_SUFFIX))
        else {
            continue;
        };
        let path = entry.path();
        if !entry.file_type().map_err(io_failed(&path))?.is_file()
            || named_version(label).is_some_and(|version| version < MEDIA_SCHEMA)
        {
            continue;
        }
        backup_file_references(&path, into).map_err(failed(&path))?;
    }
    Ok(())
}

/// The schema a migration backup's name records: `.v<N>` of
/// `caudra.db.v<N>.bak`.
fn named_version(label: &str) -> Option<i64> {
    label.strip_prefix(BACKUP_VERSION_PREFIX)?.parse().ok()
}

fn backup_file_references(path: &Path, into: &mut HashSet<MediaHash>) -> Result<(), SessionError> {
    let sidecar = sidecar_path(path);
    let modified = |path: &Path| fs::metadata(path).and_then(|metadata| metadata.modified());
    let backup_modified = modified(path).map_err(StorageError::Io)?;
    if modified(&sidecar).is_ok_and(|cached| cached >= backup_modified) {
        for line in fs::read_to_string(&sidecar)
            .map_err(StorageError::Io)?
            .lines()
        {
            into.insert(line.parse().map_err(StorageError::from)?);
        }
        return Ok(());
    }
    let connection = Connection::open_with_flags(
        immutable_database_uri(path)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let mut found = HashSet::new();
    if version >= MEDIA_SCHEMA {
        database_references(&connection, &mut found)?;
    }
    let settled = backup_modified
        .elapsed()
        .is_ok_and(|age| age >= BACKUP_SETTLE);
    if settled {
        let mut hashes: Vec<&str> = found.iter().map(MediaHash::as_str).collect();
        hashes.sort_unstable();
        let mut cached = hashes.join("\n");
        if !cached.is_empty() {
            cached.push('\n');
        }
        atomic_write_permissions(&sidecar, cached.as_bytes(), SIDECAR_MODE)?;
    }
    into.extend(found);
    Ok(())
}

fn sidecar_path(backup: &Path) -> PathBuf {
    let mut sidecar = backup.as_os_str().to_os_string();
    sidecar.push(SIDECAR_SUFFIX);
    sidecar.into()
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::OpenOptions;
    use std::io::Cursor;
    use std::time::Instant;

    use image::{ImageFormat, RgbImage};
    use rusqlite::backup::Backup;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::id::CaudraId;

    const PREVIOUS_SCHEMA: i64 = 43;
    const RED: &str = "<red>";
    const BLUE: &str = "<blue>";
    const EMPTY_METADATA: &str = "{}";
    const MEDIA_TAG: &str = r#""type":"media""#;
    const RED_WIDTH: u32 = 1;
    const BLUE_WIDTH: u32 = 2;
    const INVOCATION: &str = "invocation";
    const TOOL_ID: &str = "tool";
    const STASH_KEY: &str = "input.stash";
    const GLOBAL_SCOPE: &str = "global";
    const PDF: &str = "application/pdf";
    const UNDECODABLE: &str = "!!!";
    const AN_HOUR: Duration = Duration::from_secs(60 * 60);
    const MEASURED_STATE_VAR: &str = "CAUDRA_MEDIA_MIGRATION_STATE";
    const LOGICAL_BYTES: &str = "length(CAST(token_usage AS BLOB)) + length(CAST(metadata AS BLOB)) + length(CAST(workspace_binding AS BLOB)) + (SELECT coalesce(sum(byte_count), 0) FROM main_history_items WHERE session_id = sessions.id) + (SELECT coalesce(sum(byte_count), 0) FROM tool_outputs WHERE session_id = sessions.id) + (SELECT coalesce(sum(byte_count), 0) FROM subagent_history_items WHERE session_id = sessions.id) + (SELECT coalesce(sum(length(CAST(task_spec AS BLOB))), 0) FROM subagent_streams WHERE session_id = sessions.id)";
    const BYTES_CONSISTENT: &str =
        "logical_bytes must stay the sum of what the session's rows hold";
    const INPUT_UNTOUCHED: &str = "a tool call's input must stay byte for byte";
    const RERUN_IS_NOOP: &str = "rerunning the step must change nothing";
    const BACKUP_KEEPS_INLINE: &str = "the pre-migration backup must keep the inline images";
    const DELETED_NOTHING: &str = "a root that cannot be read must delete nothing";
    const SIDECAR_BYTES: &[u8] = b"sidecar";
    const BACKUP_BYTES: &[u8] = b"backup";
    const CACHED_BACKUP: &[u8] = b"never opened while its sidecar exists";

    fn png(width: u32) -> String {
        let mut bytes = Vec::new();
        RgbImage::new(width, 1)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        BASE64.encode(bytes)
    }

    fn inline(media_type: &str, data: &str) -> String {
        format!(r#"{{"type":"base64","media_type":"{media_type}","data":"{data}"}}"#)
    }

    fn legacy(data: &str) -> String {
        format!(r#"{{"media_type":"image/png","data":"{data}"}}"#)
    }

    fn reference(data: &str) -> String {
        let temp = TempDir::new().unwrap();
        let media = MediaStore::new(&StateDir::from_path(temp.path().to_path_buf()))
            .put(&BASE64.decode(data).unwrap())
            .unwrap();
        serde_json::to_string(&StoredImage::Media {
            media_type: "image/png".into(),
            media,
        })
        .unwrap()
    }

    fn hash_of(reference: &str) -> MediaHash {
        let StoredImage::Media { media, .. } = serde_json::from_str(reference).unwrap() else {
            unreachable!("a reference names a stored file")
        };
        media.sha256
    }

    fn age(path: &Path, by: Duration) {
        OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    }

    fn insert_session(connection: &Connection, id: CaudraId, metadata: &str) {
        connection
            .execute(
                "INSERT INTO sessions (id, format_version, title, cwd, model, created_at, \
                 updated_at, token_usage, metadata) VALUES (?1, 1, 't', '/w', 'm', 0, 0, '{}', ?2)",
                params![id.as_bytes().as_slice(), metadata],
            )
            .unwrap();
    }

    /// Every payload the fixture writes, as `(before, after)`.
    struct Payloads {
        tool_input: String,
        history: [(String, String); 3],
        tool_output: (String, String),
        transcript: (String, String),
        metadata: (String, String),
        stash: (String, String),
    }

    impl Payloads {
        fn new() -> Self {
            let (red, blue) = (png(RED_WIDTH), png(BLUE_WIDTH));
            let (red_ref, blue_ref) = (reference(&red), reference(&blue));
            let both = |template: &str| {
                (
                    template
                        .replace(RED, &inline("image/png", &red))
                        .replace(BLUE, &legacy(&blue)),
                    template.replace(RED, &red_ref).replace(BLUE, &blue_ref),
                )
            };
            let tool_input = format!(
                r#"{{"image":{},"nested":[{}]}}"#,
                inline("image/png", &red),
                legacy(&blue)
            );
            let tool_call = format!(
                r#"{{"type":"tool_call","id":"c","name":"view","input":{tool_input},"images":[<red>]}}"#
            );
            let unconverted = format!(
                r#"{{"type":"assistant","text":"say \"data\":\"x\"","pdf":{},"broken":{}}}"#,
                inline(PDF, &red),
                inline("image/png", UNDECODABLE)
            );
            Self {
                tool_input,
                history: [
                    both(r#"{"type":"user","text":"look","images":[<red>,<blue>]}"#),
                    both(&tool_call),
                    (unconverted.clone(), unconverted),
                ],
                tool_output: both(r#"{"Image":{"source":<red>,"text":"[image]"}}"#),
                transcript: both(
                    r#"[{"role":"user","content":[{"type":"image","source":<blue>},{"type":"text","text":"x"}]}]"#,
                ),
                metadata: both(
                    r#"{"input_draft_images":[<blue>],"queued_messages":[{"text":"q","images":[<blue>]}]}"#,
                ),
                stash: both(r#"[{"id":"s","text":"t","images":[<blue>],"created_at":1}]"#),
            }
        }
    }

    fn write_fixture(connection: &Connection, session: CaudraId, payloads: &Payloads) {
        let id = session.as_bytes().as_slice();
        insert_session(connection, session, &payloads.metadata.0);
        for (ordinal, (payload, _)) in payloads.history.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO main_history_items (session_id, ordinal, payload, byte_count) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        id,
                        ordinal as i64,
                        compress_payload(payload).unwrap(),
                        payload.len() as i64
                    ],
                )
                .unwrap();
        }
        let output = &payloads.tool_output.0;
        connection
            .execute(
                "INSERT INTO tool_outputs (session_id, tool_id, payload, byte_count) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    id,
                    TOOL_ID,
                    compress_payload(output).unwrap(),
                    output.len() as i64
                ],
            )
            .unwrap();
        let task = r#"{"sequence":1,"task_id":"t"}"#;
        connection
            .execute(
                "INSERT INTO background_tasks (session_id, invocation_id, payload, bytes) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, INVOCATION, task, task.len() as i64],
            )
            .unwrap();
        let transcript = &payloads.transcript.0;
        connection
            .execute(
                "INSERT INTO background_transcripts (session_id, invocation_id, payload, byte_count) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, INVOCATION, compress_payload(transcript).unwrap(), transcript.len() as i64],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO state (scope, key, value, updated_at) VALUES (?1, ?2, ?3, 0)",
                params![GLOBAL_SCOPE, STASH_KEY, payloads.stash.0],
            )
            .unwrap();
        connection
            .execute(
                &format!("UPDATE sessions SET logical_bytes = {LOGICAL_BYTES}"),
                [],
            )
            .unwrap();
    }

    fn stored(connection: &Connection) -> Vec<String> {
        let mut payloads = Vec::new();
        for (table, order) in [
            ("main_history_items", "ordinal"),
            ("tool_outputs", "tool_id"),
            ("background_transcripts", "invocation_id"),
        ] {
            let mut select = connection
                .prepare(&format!(
                    "SELECT payload, byte_count FROM {table} ORDER BY {order}"
                ))
                .unwrap();
            let mut rows = select.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let payload =
                    decompress_payload(&row.get::<_, Vec<u8>>(0).unwrap(), table).unwrap();
                assert_eq!(row.get::<_, i64>(1).unwrap(), payload.len() as i64);
                payloads.push(payload);
            }
        }
        payloads.push(
            connection
                .query_row("SELECT metadata FROM sessions", [], |row| row.get(0))
                .unwrap(),
        );
        payloads.push(
            connection
                .query_row(
                    "SELECT value FROM state WHERE key = ?1",
                    [STASH_KEY],
                    |row| row.get(0),
                )
                .unwrap(),
        );
        payloads
    }

    fn expected(payloads: &Payloads, pick: fn(&(String, String)) -> &String) -> Vec<String> {
        payloads
            .history
            .iter()
            .map(pick)
            .chain([
                pick(&payloads.tool_output),
                pick(&payloads.transcript),
                pick(&payloads.metadata),
                pick(&payloads.stash),
            ])
            .cloned()
            .collect()
    }

    fn logical_bytes_exact(connection: &Connection) -> bool {
        connection
            .query_row(
                &format!("SELECT logical_bytes = {LOGICAL_BYTES} FROM sessions"),
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn media_files(state: &StateDir) -> usize {
        let root = state.path().join(crate::media::MEDIA_DIR);
        walk(&root)
    }

    fn walk(dir: &Path) -> usize {
        fs::read_dir(dir).map_or(0, |entries| {
            entries
                .map(|entry| entry.unwrap().path())
                .map(|path| if path.is_dir() { walk(&path) } else { 1 })
                .sum()
        })
    }

    fn reopen_from(state: &StateDir, version: i64) -> SessionDatabase {
        let database = SessionDatabase::open_state(state).unwrap();
        database
            .connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(database);
        SessionDatabase::open_state(state).unwrap()
    }

    #[test]
    fn migration_moves_inline_images_into_the_store_and_keeps_inputs() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let payloads = Payloads::new();
        let database = SessionDatabase::open_state(&state).unwrap();
        write_fixture(&database.connection, CaudraId::generate(), &payloads);
        drop(database);

        let migrated = reopen_from(&state, PREVIOUS_SCHEMA);

        assert_eq!(
            stored(&migrated.connection),
            expected(&payloads, |pair| &pair.1)
        );
        assert!(
            payloads.history[1].1.contains(&payloads.tool_input),
            "{INPUT_UNTOUCHED}"
        );
        assert!(
            logical_bytes_exact(&migrated.connection),
            "{BYTES_CONSISTENT}"
        );
        assert_eq!(media_files(&state), 2);
        let store = MediaStore::new(&state);
        for data in [png(RED_WIDTH), png(BLUE_WIDTH)] {
            let hash = MediaHash::of(&BASE64.decode(&data).unwrap());
            assert_eq!(store.base64(&hash).unwrap().as_deref(), Some(data.as_str()));
        }
        let backup = Connection::open(
            state
                .path()
                .join(BACKUPS_DIR)
                .join(format!("{SESSIONS_DB_FILE}.v{PREVIOUS_SCHEMA}.bak")),
        )
        .unwrap();
        assert_eq!(
            stored(&backup),
            expected(&payloads, |pair| &pair.0),
            "{BACKUP_KEEPS_INLINE}"
        );
    }

    #[test]
    fn rerunning_the_migration_changes_nothing() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let payloads = Payloads::new();
        let database = SessionDatabase::open_state(&state).unwrap();
        write_fixture(&database.connection, CaudraId::generate(), &payloads);
        drop(database);
        let first = stored(&reopen_from(&state, PREVIOUS_SCHEMA).connection);

        let rerun = reopen_from(&state, PREVIOUS_SCHEMA);

        assert_eq!(stored(&rerun.connection), first, "{RERUN_IS_NOOP}");
        assert!(logical_bytes_exact(&rerun.connection), "{BYTES_CONSISTENT}");
        assert_eq!(media_files(&state), 2);
    }

    #[test]
    fn a_rewrite_over_the_pending_budget_resumes_after_its_last_key() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let database = SessionDatabase::open_state(&state).unwrap();
        let session = CaudraId::generate();
        insert_session(&database.connection, session, EMPTY_METADATA);
        let rows: u32 = 3;
        for width in 1..=rows {
            let payload = format!(r#"{{"images":[{}]}}"#, inline("image/png", &png(width)));
            database
                .connection
                .execute(
                    "INSERT INTO main_history_items (session_id, ordinal, payload, byte_count) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        session.as_bytes().as_slice(),
                        width,
                        compress_payload(&payload).unwrap(),
                        payload.len() as i64
                    ],
                )
                .unwrap();
        }
        let transaction = database.connection.unchecked_transaction().unwrap();

        rewrite_column(&transaction, &IMAGE_COLUMNS[0], &MediaStore::new(&state), 1).unwrap();

        let mut select = transaction
            .prepare("SELECT payload FROM main_history_items")
            .unwrap();
        let payloads: Vec<String> = select
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|stored| decompress_payload(&stored.unwrap(), "payload").unwrap())
            .collect();
        assert_eq!(payloads.len(), rows as usize);
        assert!(payloads.iter().all(|payload| payload.contains(MEDIA_TAG)));
        assert_eq!(media_files(&state), rows as usize);
    }

    #[test_case(r#"{"input":{"media_type":"image/png","data":"x"}}"#, 0; "tool input is skipped")]
    #[test_case(r#"[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"x"}}]"#, 1; "message content image")]
    #[test_case(r#"{"text":"a\"b","media_type":"image/png","data":"x"}"#, 0; "extra key")]
    #[test_case(r#"{"media_type":"image/png","data":{"nested":"x"}}"#, 0; "non string member")]
    #[test_case(r#" { "media_type" : "image/png" , "data" : "x" } "#, 1; "whitespace")]
    fn image_shaped_objects_are_found_outside_inputs(payload: &str, found: usize) {
        assert_eq!(image_shaped_objects(payload).unwrap().len(), found);
    }

    #[test_case(r#"{"data":"x""#; "unterminated object")]
    #[test_case(r#"{"data":"x}"#; "unterminated string")]
    #[test_case(r#"{"data":"x\"#; "string ending in an escape")]
    #[test_case(r#"{} {}"#; "trailing value")]
    fn malformed_payloads_are_refused(payload: &str) {
        assert!(matches!(
            image_shaped_objects(payload),
            Err(ScanError::Malformed(_))
        ));
    }

    struct PruneFixture {
        _temp: TempDir,
        state: StateDir,
        database: SessionDatabase,
        store: MediaStore,
    }

    impl PruneFixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let state = StateDir::from_path(temp.path().to_path_buf());
            let database = SessionDatabase::open_state(&state).unwrap();
            let store = MediaStore::new(&state);
            Self {
                _temp: temp,
                state,
                database,
                store,
            }
        }

        /// An old file, and the reference a payload would hold to it.
        fn old_file(&self, bytes: &[u8]) -> (PathBuf, String) {
            let media = self.store.put(bytes).unwrap();
            let path = self.store.path(&media.sha256);
            age(&path, MEDIA_GRACE + AN_HOUR);
            let reference = serde_json::to_string(&StoredImage::Media {
                media_type: "image/png".into(),
                media,
            })
            .unwrap();
            (path, reference)
        }

        fn backups(&self) -> PathBuf {
            let backups = self.state.path().join(BACKUPS_DIR);
            fs::create_dir_all(&backups).unwrap();
            backups
        }
    }

    #[test]
    fn prune_keeps_every_root_and_deletes_the_rest() {
        let fixture = PruneFixture::new();
        let (in_metadata, metadata_ref) = fixture.old_file(b"metadata");
        let (in_stash, stash_ref) = fixture.old_file(b"stash");
        let (in_archive, archive_ref) = fixture.old_file(b"archive");
        let (in_sidecar, sidecar_ref) = fixture.old_file(SIDECAR_BYTES);
        let (in_backup, backup_ref) = fixture.old_file(BACKUP_BYTES);
        let (unreferenced, _) = fixture.old_file(b"unreferenced");
        let connection = &fixture.database.connection;
        insert_session(
            connection,
            CaudraId::generate(),
            &format!(r#"{{"input_draft_images":[{metadata_ref}]}}"#),
        );
        connection
            .execute(
                "INSERT INTO state (scope, key, value, updated_at) VALUES (?1, ?2, ?3, 0)",
                params![
                    GLOBAL_SCOPE,
                    STASH_KEY,
                    format!(r#"[{{"images":[{stash_ref}]}}]"#)
                ],
            )
            .unwrap();
        let archive = fixture
            .state
            .path()
            .join(SESSIONS_DIR)
            .join(ARCHIVE_DIR)
            .join(CaudraId::generate().to_string());
        fs::create_dir_all(&archive).unwrap();
        fs::write(
            archive.join("1.jsonl"),
            format!("{{\"images\":[{archive_ref}]}}\n"),
        )
        .unwrap();
        let backups = fixture.backups();
        let cached = backups.join(format!("{SESSIONS_DB_FILE}.v{MEDIA_SCHEMA}.bak"));
        fs::write(&cached, CACHED_BACKUP).unwrap();
        fs::write(
            sidecar_path(&cached),
            format!("{}\n{}\n", hash_of(&metadata_ref), hash_of(&sidecar_ref)),
        )
        .unwrap();
        fs::write(
            backups.join(format!("{SESSIONS_DB_FILE}.v{PREVIOUS_SCHEMA}.bak")),
            b"inline",
        )
        .unwrap();
        let (_source_temp, source) = {
            let temp = TempDir::new().unwrap();
            let state = StateDir::from_path(temp.path().to_path_buf());
            (temp, state)
        };
        let source = SessionDatabase::open_state(&source).unwrap();
        insert_session(
            &source.connection,
            CaudraId::generate(),
            &format!(r#"{{"queued_messages":[{{"images":[{backup_ref}]}}]}}"#),
        );
        let review = backups.join(format!("{SESSIONS_DB_FILE}.permission-review-x.bak"));
        let mut destination = Connection::open(&review).unwrap();
        Backup::new(&source.connection, &mut destination)
            .unwrap()
            .run_to_completion(64, Duration::ZERO, None)
            .unwrap();
        drop(destination);
        age(&review, BACKUP_SETTLE + AN_HOUR);

        let report = fixture.database.prune_unreferenced_media(false).unwrap();

        assert_eq!(report.unreferenced.files, 1);
        assert_eq!(
            report.backup_only,
            MediaPruneReport {
                files: 2,
                bytes: (SIDECAR_BYTES.len() + BACKUP_BYTES.len()) as u64,
            }
        );
        assert!(!unreferenced.exists());
        for kept in [in_metadata, in_stash, in_archive, in_sidecar, in_backup] {
            assert!(kept.exists(), "{}", kept.display());
        }
        let cached_review = fs::read_to_string(sidecar_path(&review)).unwrap();
        assert!(backup_ref.contains(cached_review.trim()));
    }

    #[test]
    fn prune_deletes_nothing_when_a_backup_cannot_be_read() {
        let fixture = PruneFixture::new();
        let (unreferenced, _) = fixture.old_file(b"unreferenced");
        let broken = fixture
            .backups()
            .join(format!("{SESSIONS_DB_FILE}.permission-review-x.bak"));
        fs::write(&broken, b"not a database").unwrap();

        let error = fixture
            .database
            .prune_unreferenced_media(false)
            .unwrap_err();

        assert_eq!(error.root(), "backup");
        assert!(unreferenced.exists(), "{DELETED_NOTHING}");
        assert!(!sidecar_path(&broken).exists());
    }

    #[test]
    fn a_backup_rewritten_after_its_sidecar_is_read_again() {
        let fixture = PruneFixture::new();
        let (unreferenced, _) = fixture.old_file(b"unreferenced");
        let rewritten = fixture
            .backups()
            .join(format!("{SESSIONS_DB_FILE}.v{MEDIA_SCHEMA}.bak"));
        fs::write(&rewritten, b"not a database").unwrap();
        fs::write(sidecar_path(&rewritten), b"").unwrap();
        age(&sidecar_path(&rewritten), AN_HOUR);

        let error = fixture
            .database
            .prune_unreferenced_media(false)
            .unwrap_err();

        assert_eq!(error.root(), "backup");
        assert!(unreferenced.exists(), "{DELETED_NOTHING}");
    }

    /// Migrates a copy of a real state directory at schema 43, named by
    /// `CAUDRA_MEDIA_MIGRATION_STATE`, and reports what moved. Never point it
    /// at a live state directory.
    #[test]
    #[ignore = "needs a copied state directory"]
    fn measure_migration_of_a_copied_state_directory() {
        let state = StateDir::from_path(PathBuf::from(env::var(MEASURED_STATE_VAR).unwrap()));
        let database_path = state.path().join(SESSIONS_DB_FILE);
        let before = fs::metadata(&database_path).unwrap().len();
        let started = Instant::now();

        let database = SessionDatabase::open_state(&state).unwrap();

        let elapsed = started.elapsed();
        let scan_started = Instant::now();
        let unreferenced = database.prune_unreferenced_media(true).unwrap();
        let scan = scan_started.elapsed();
        for column in &IMAGE_COLUMNS {
            let mut select = database
                .connection
                .prepare(&format!("SELECT {} FROM {}", column.payload, column.table))
                .unwrap();
            let mut rows = select.query([]).unwrap();
            let mut holding = 0;
            while let Some(row) = rows.next().unwrap() {
                holding += usize::from(column.read(row, 0).unwrap().contains(MEDIA_TAG));
            }
            println!("{} rows_with_media={holding}", column.table);
        }
        let freelist: i64 = database
            .connection
            .pragma_query_value(None, "freelist_count", |row| row.get(0))
            .unwrap();
        database.connection.execute_batch("VACUUM").unwrap();
        let after = fs::metadata(&database_path).unwrap().len();
        let media_root = state.path().join(crate::media::MEDIA_DIR);
        println!(
            "before={before} after_vacuum={after} freelist_pages_before_vacuum={freelist} \
             media_files={} media_bytes={} migration={elapsed:?} \
             cleanup_dry_run={scan:?} unreferenced={unreferenced:?}",
            walk(&media_root),
            directory_bytes(&media_root),
        );
    }

    fn directory_bytes(dir: &Path) -> u64 {
        fs::read_dir(dir).map_or(0, |entries| {
            entries
                .map(|entry| entry.unwrap().path())
                .map(|path| {
                    if path.is_dir() {
                        directory_bytes(&path)
                    } else {
                        fs::metadata(&path).unwrap().len()
                    }
                })
                .sum()
        })
    }

    #[test]
    fn a_dry_run_counts_without_deleting() {
        let fixture = PruneFixture::new();
        let (unreferenced, _) = fixture.old_file(b"unreferenced");

        let report = fixture.database.prune_unreferenced_media(true).unwrap();

        assert_eq!(report.unreferenced.files, 1);
        assert!(unreferenced.exists());
    }

    #[test_case(true; "a dry run counts it")]
    #[test_case(false; "a prune deletes it")]
    fn the_reference_cache_of_a_deleted_backup_goes(dry_run: bool) {
        let fixture = PruneFixture::new();
        let backups = fixture.backups();
        let deleted = backups.join(format!("{SESSIONS_DB_FILE}.permission-review-x.bak"));
        fs::write(sidecar_path(&deleted), b"").unwrap();
        let present = backups.join(format!("{SESSIONS_DB_FILE}.v{MEDIA_SCHEMA}.bak"));
        fs::write(&present, CACHED_BACKUP).unwrap();
        fs::write(sidecar_path(&present), b"").unwrap();

        let report = fixture.database.prune_unreferenced_media(dry_run).unwrap();

        assert_eq!(report.orphan_sidecars, 1);
        assert_eq!(sidecar_path(&deleted).exists(), dry_run);
        assert!(sidecar_path(&present).exists());
    }

    #[test]
    fn check_names_missing_and_corrupt_media() {
        let fixture = PruneFixture::new();
        let (_, intact) = fixture.old_file(b"intact");
        let (missing_path, missing) = fixture.old_file(b"missing");
        let (corrupt_path, corrupt) = fixture.old_file(b"corrupt");
        fs::remove_file(missing_path).unwrap();
        fs::write(corrupt_path, b"tampered").unwrap();
        insert_session(
            &fixture.database.connection,
            CaudraId::generate(),
            &format!(r#"{{"input_draft_images":[{intact},{missing},{corrupt}]}}"#),
        );

        let report = fixture.database.check_media().unwrap();

        assert_eq!(
            report,
            MediaCheckReport {
                referenced: 3,
                missing: vec![hash_of(&missing)],
                corrupt: vec![hash_of(&corrupt)],
            }
        );
    }
}
