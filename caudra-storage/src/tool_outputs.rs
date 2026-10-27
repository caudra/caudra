//! Tool outputs saved in full when the model sees only a bounded preview. Each
//! one is a zstd row of `saved_tool_outputs`, owned by its session through a
//! foreign key, so deleting the session deletes its outputs.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use regex::Regex;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::id::CaudraId;
use crate::sessions::{
    RuntimeRetry, SessionDatabase, SessionError, compress_payload, from_i64, from_i64_usize,
    id_from_row, to_i64,
};
use crate::{
    DESCRIPTIVE_ID_ATTEMPTS, DESCRIPTIVE_ID_MAX_LEN, DescriptiveIdCandidates, StateDir,
    StorageError,
};

mod legacy;
mod text;

pub(crate) use legacy::{import_legacy_outputs, retire_legacy_outputs};

pub(crate) const SAVED_OUTPUTS_TABLE: &str = "saved_tool_outputs";
pub(crate) const SAVED_OUTPUTS_SCHEMA: &str = r#"
CREATE TABLE saved_tool_outputs (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    output_id  TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    byte_count INTEGER NOT NULL,
    payload    BLOB NOT NULL,
    UNIQUE(session_id, output_id)
) STRICT;
"#;
/// Small enough that zstd's worst case for incompressible text still fits the
/// 40 MiB value limit every session database connection sets.
pub(crate) const DEFAULT_MAX_STORED_BYTES: usize = 32 * 1024 * 1024;
pub const TOOL_OUTPUT_CONTROL_RESERVE_BYTES: usize = 256;
const MAX_GREP_PATTERN_CHARS: usize = 512;
const MAX_GREP_MATCHES: usize = 200;
const MAX_GREP_CONTEXT: usize = 5;
pub(crate) const UNREFERENCED_OUTPUT_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_OUTPUT_LABEL: &str = "output";
const PAYLOAD_FIELD: &str = "saved_tool_outputs.payload";
const SAVE_OPERATION: &str = "save tool output";
const LOAD_OPERATION: &str = "load tool output";
const DELETE_OPERATION: &str = "delete tool output";
const PRUNE_OPERATION: &str = "prune unreferenced tool outputs";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolOutputId(String);

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error(
    "invalid tool output ID: expected a lowercase ASCII alphanumeric slug with single hyphens (at most {DESCRIPTIVE_ID_MAX_LEN} bytes), or a canonical legacy ID"
)]
pub struct ToolOutputIdParseError;

impl ToolOutputId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolOutputId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ToolOutputId {
    type Err = ToolOutputIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > DESCRIPTIVE_ID_MAX_LEN {
            return Err(ToolOutputIdParseError);
        }
        let slug = value.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
        let legacy = value
            .parse::<CaudraId>()
            .is_ok_and(|id| id.to_string() == value);
        if slug || legacy {
            Ok(Self(value.to_owned()))
        } else {
            Err(ToolOutputIdParseError)
        }
    }
}

impl<'de> Deserialize<'de> for ToolOutputId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputRef {
    pub id: ToolOutputId,
    pub byte_count: usize,
    pub line_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputReadResult {
    pub text: String,
    pub offset: usize,
    pub byte_offset: usize,
    pub returned_lines: usize,
    pub next_offset: Option<usize>,
    pub next_byte_offset: usize,
    pub total_lines: usize,
    pub total_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputGrepRow {
    pub line_number: usize,
    pub text: String,
    pub is_match: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputGrepResult {
    pub rows: Vec<ToolOutputGrepRow>,
    pub next_offset: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutputPreview {
    pub head: String,
    pub tail: String,
}

impl ToolOutputPreview {
    pub fn into_text(self) -> String {
        if self.head.is_empty() {
            return self.tail;
        }
        if self.tail.is_empty() {
            return self.head;
        }
        self.head + "\n" + &self.tail
    }
}

#[derive(Debug, Error)]
pub enum ToolOutputError {
    #[error("tool output is {byte_count} bytes, exceeding the {max_bytes}-byte limit")]
    TooLarge { byte_count: usize, max_bytes: usize },
    #[error("tool output {output_id} does not exist for session {session_id}")]
    NotFound {
        session_id: CaudraId,
        output_id: ToolOutputId,
    },
    #[error("tool output {output_id} for session {session_id} is not valid UTF-8")]
    InvalidUtf8 {
        session_id: CaudraId,
        output_id: ToolOutputId,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("offset must be at least 1")]
    InvalidOffset,
    #[error("byte offset {byte_offset} is invalid for line {line_number}")]
    InvalidByteOffset {
        line_number: usize,
        byte_offset: usize,
    },
    #[error("limit must be at least 1")]
    InvalidLimit,
    #[error("grep pattern is {char_count} characters, exceeding the {max_chars}-character limit")]
    PatternTooLong { char_count: usize, max_chars: usize },
    #[error("invalid grep pattern: {0}")]
    InvalidPattern(#[from] regex::Error),
    #[error(
        "could not allocate a tool output ID after {DESCRIPTIVE_ID_ATTEMPTS} candidates; use a different label or remove unneeded session outputs"
    )]
    IdCollision,
    #[error(transparent)]
    Database(Box<SessionError>),
}

impl From<SessionError> for ToolOutputError {
    fn from(error: SessionError) -> Self {
        Self::Database(Box::new(error))
    }
}

impl From<StorageError> for ToolOutputError {
    fn from(error: StorageError) -> Self {
        SessionError::from(error).into()
    }
}

#[derive(Clone, Debug)]
pub struct ToolOutputStore {
    state_dir: StateDir,
    max_bytes: usize,
}

/// Collects one output in memory and saves it whole on `finish`. Dropping it
/// saves nothing.
#[derive(Debug)]
pub struct ToolOutputSink {
    store: ToolOutputStore,
    session_id: CaudraId,
    label: String,
    text: String,
}

impl ToolOutputSink {
    pub fn append(&mut self, text: &str) -> Result<(), ToolOutputError> {
        self.append_with_reserve(text, 0)
    }

    pub fn append_with_reserve(
        &mut self,
        text: &str,
        reserve_bytes: usize,
    ) -> Result<(), ToolOutputError> {
        let byte_count = self.text.len().saturating_add(text.len());
        let max_bytes = self.store.max_bytes.saturating_sub(reserve_bytes);
        if byte_count > max_bytes {
            return Err(ToolOutputError::TooLarge {
                byte_count,
                max_bytes,
            });
        }
        self.text.push_str(text);
        Ok(())
    }

    pub fn finish(self) -> Result<ToolOutputRef, ToolOutputError> {
        self.store
            .insert(self.session_id, &self.text, &self.label, &runtime_retry())
    }
}

fn never_cancelled() -> bool {
    false
}

fn runtime_retry() -> RuntimeRetry<'static> {
    RuntimeRetry::new(None, &never_cancelled)
}

impl ToolOutputStore {
    pub fn state_dir(&self) -> &StateDir {
        &self.state_dir
    }

    pub fn new(state_dir: StateDir) -> Self {
        Self::with_max_bytes(state_dir, DEFAULT_MAX_STORED_BYTES)
    }

    pub fn with_max_bytes(state_dir: StateDir, max_bytes: usize) -> Self {
        Self {
            state_dir,
            max_bytes,
        }
    }

    pub fn put(&self, session_id: CaudraId, text: &str) -> Result<ToolOutputRef, ToolOutputError> {
        self.put_named(session_id, text, DEFAULT_OUTPUT_LABEL)
    }

    pub fn put_named(
        &self,
        session_id: CaudraId,
        text: &str,
        label: &str,
    ) -> Result<ToolOutputRef, ToolOutputError> {
        self.put_named_within(session_id, text, label, &runtime_retry())
    }

    /// Saves under the caller's contention budget, so a write that waits on
    /// another writer honours the caller's deadline and cancellation.
    pub fn put_named_within(
        &self,
        session_id: CaudraId,
        text: &str,
        label: &str,
        retry: &RuntimeRetry<'_>,
    ) -> Result<ToolOutputRef, ToolOutputError> {
        self.ensure_size(text.len())?;
        self.insert(session_id, text, label, retry)
    }

    pub fn begin(&self, session_id: CaudraId) -> ToolOutputSink {
        self.begin_named(session_id, DEFAULT_OUTPUT_LABEL)
    }

    pub fn begin_named(&self, session_id: CaudraId, label: &str) -> ToolOutputSink {
        ToolOutputSink {
            store: self.clone(),
            session_id,
            label: label.to_owned(),
            text: String::new(),
        }
    }

    pub fn read(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        offset: usize,
        limit: usize,
    ) -> Result<ToolOutputReadResult, ToolOutputError> {
        self.read_at(session_id, id, offset, limit, 0)
    }

    pub fn read_at(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        offset: usize,
        limit: usize,
        byte_offset: usize,
    ) -> Result<ToolOutputReadResult, ToolOutputError> {
        validate_page(offset, limit)?;
        let text = self.load_text(session_id, id)?;
        text::read_page(&text, offset, limit, byte_offset)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn grep(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        pattern: &str,
        offset: usize,
        limit: usize,
        context_before: usize,
        context_after: usize,
    ) -> Result<ToolOutputGrepResult, ToolOutputError> {
        validate_page(offset, limit)?;
        let char_count = pattern.chars().count();
        if char_count > MAX_GREP_PATTERN_CHARS {
            return Err(ToolOutputError::PatternTooLong {
                char_count,
                max_chars: MAX_GREP_PATTERN_CHARS,
            });
        }
        let regex = Regex::new(pattern)?;
        let text = self.load_text(session_id, id)?;
        Ok(text::grep(
            &text,
            &regex,
            text::GrepWindow {
                start: offset - 1,
                limit: limit.min(MAX_GREP_MATCHES),
                before: context_before.min(MAX_GREP_CONTEXT),
                after: context_after.min(MAX_GREP_CONTEXT),
            },
        ))
    }

    pub fn preview(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<ToolOutputPreview, ToolOutputError> {
        if max_lines == 0 || max_bytes == 0 {
            return Ok(ToolOutputPreview {
                head: String::new(),
                tail: String::new(),
            });
        }
        let text = self.load_text(session_id, id)?;
        Ok(text::preview(&text, max_lines, max_bytes))
    }

    pub fn load_text(
        &self,
        session_id: CaudraId,
        id: ToolOutputId,
    ) -> Result<String, ToolOutputError> {
        let retry = runtime_retry();
        let database = SessionDatabase::open_runtime(&self.state_dir, &retry)?;
        let Some((byte_count, payload)) =
            retry.run(LOAD_OPERATION, || database.saved_output(session_id, &id))?
        else {
            return Err(ToolOutputError::NotFound {
                session_id,
                output_id: id,
            });
        };
        self.ensure_size(byte_count)?;
        let bytes = zstd::bulk::decompress(&payload, byte_count)
            .ok()
            .filter(|bytes| bytes.len() == byte_count)
            .ok_or_else(|| SessionError::CorruptDatabaseValue {
                field: PAYLOAD_FIELD,
                reason: format!("output {id} does not decode to its {byte_count} bytes"),
            })?;
        String::from_utf8(bytes).map_err(|error| ToolOutputError::InvalidUtf8 {
            session_id,
            output_id: id,
            source: error.utf8_error(),
        })
    }

    /// Removes one saved output, so nothing can read it back. An output that is
    /// already gone is not an error.
    pub fn delete(&self, session_id: CaudraId, id: &ToolOutputId) -> Result<(), ToolOutputError> {
        let retry = runtime_retry();
        let database = SessionDatabase::open_runtime(&self.state_dir, &retry)?;
        database.runtime_transaction(&retry, DELETE_OPERATION, |transaction| {
            transaction.execute(
                "DELETE FROM saved_tool_outputs WHERE session_id = ?1 AND output_id = ?2",
                params![session_id.as_bytes().as_slice(), id.as_str()],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Deletes outputs no session record references any more once they are
    /// older than the grace period, or with `dry_run` counts them. Returns how
    /// many.
    pub fn prune_unreferenced(&self, dry_run: bool) -> Result<u64, ToolOutputError> {
        let retry = runtime_retry();
        let database = SessionDatabase::open_runtime(&self.state_dir, &retry)?;
        Ok(retry.run(PRUNE_OPERATION, || {
            database.prune_unreferenced_outputs(UNREFERENCED_OUTPUT_GRACE, dry_run)
        })?)
    }

    fn insert(
        &self,
        session_id: CaudraId,
        text: &str,
        label: &str,
        retry: &RuntimeRetry<'_>,
    ) -> Result<ToolOutputRef, ToolOutputError> {
        let payload = compress_payload(text)?;
        let byte_count = to_i64(text.len(), "tool output bytes")?;
        let database = SessionDatabase::open_runtime(&self.state_dir, retry)?;
        let id = database.runtime_transaction(retry, SAVE_OPERATION, |transaction| {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO saved_tool_outputs \
                     (session_id, output_id, created_at, byte_count, payload) \
                 VALUES (?1, ?2, unixepoch(), ?3, ?4) \
                 ON CONFLICT(session_id, output_id) DO NOTHING",
            )?;
            for candidate in DescriptiveIdCandidates::new(label, DEFAULT_OUTPUT_LABEL) {
                let inserted = insert.execute(params![
                    session_id.as_bytes().as_slice(),
                    &candidate,
                    byte_count,
                    &payload
                ])?;
                if inserted == 1 {
                    return Ok(Some(ToolOutputId(candidate)));
                }
            }
            Ok(None)
        })?;
        let id = id.ok_or(ToolOutputError::IdCollision)?;
        Ok(ToolOutputRef {
            id,
            byte_count: text.len(),
            line_count: line_count(text),
        })
    }

    fn ensure_size(&self, byte_count: usize) -> Result<(), ToolOutputError> {
        if byte_count > self.max_bytes {
            return Err(ToolOutputError::TooLarge {
                byte_count,
                max_bytes: self.max_bytes,
            });
        }
        Ok(())
    }
}

impl SessionDatabase {
    fn saved_output(
        &self,
        session_id: CaudraId,
        id: &ToolOutputId,
    ) -> Result<Option<(usize, Vec<u8>)>, SessionError> {
        let row = self
            .connection()
            .query_row(
                "SELECT byte_count, payload FROM saved_tool_outputs \
                 WHERE session_id = ?1 AND output_id = ?2",
                params![session_id.as_bytes().as_slice(), id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        row.map(|(byte_count, payload)| {
            Ok((
                from_i64_usize(byte_count, "saved tool output bytes")?,
                payload,
            ))
        })
        .transpose()
    }

    /// See [`ToolOutputStore::prune_unreferenced`].
    pub(crate) fn prune_unreferenced_outputs(
        &self,
        grace: Duration,
        dry_run: bool,
    ) -> Result<u64, SessionError> {
        let grace = i64::try_from(grace.as_secs()).unwrap_or(i64::MAX);
        let sessions = {
            let mut statement = self.connection().prepare(
                "SELECT DISTINCT session_id FROM saved_tool_outputs \
                 WHERE created_at <= unixepoch() - ?1",
            )?;
            let mut rows = statement.query([grace])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                sessions.push(id_from_row(row, 0)?);
            }
            sessions
        };
        let mut pruned = 0;
        for session_id in sessions {
            let Some(referenced) = self.referenced_outputs(session_id) else {
                continue;
            };
            let filter = "FROM saved_tool_outputs WHERE session_id = ?1 \
                 AND created_at <= unixepoch() - ?2 \
                 AND output_id NOT IN (SELECT value FROM json_each(?3))";
            let arguments = params![
                session_id.as_bytes().as_slice(),
                grace,
                id_list(referenced.iter())?
            ];
            pruned += if dry_run {
                from_i64(
                    self.connection().query_row(
                        &format!("SELECT count(*) {filter}"),
                        arguments,
                        |row| row.get(0),
                    )?,
                    "unreferenced tool outputs",
                )?
            } else {
                self.connection()
                    .execute(&format!("DELETE {filter}"), arguments)? as u64
            };
        }
        Ok(pruned)
    }

    /// Every output `session_id`'s records and recovery archives reference.
    /// `None` is fail-closed: the caller keeps every output whenever a record
    /// cannot be read or parsed.
    fn referenced_outputs(&self, session_id: CaudraId) -> Option<HashSet<ToolOutputId>> {
        let mut referenced = HashSet::new();
        let mut valid = true;
        self.visit_payload_json(session_id, |payload| match serde_json::from_str(payload) {
            Ok(value) => collect_output_ids(&value, &mut referenced),
            Err(_) => valid = false,
        })
        .ok()?;
        if !valid {
            return None;
        }
        let archive_dir = self
            .state_dir()
            .path()
            .join(crate::sessions::SESSIONS_DIR)
            .join(crate::sessions::ARCHIVE_DIR)
            .join(session_id.to_string());
        match fs::read_dir(archive_dir) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry.ok()?.path();
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "jsonl")
                    {
                        collect_references_from_archive(&path, &mut referenced)?;
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        Some(referenced)
    }
}

/// Copies `ids` from `source` into `target` inside a fork's first save, which
/// fails whole when any of them is gone.
pub(crate) fn copy_saved_outputs(
    transaction: &Transaction<'_>,
    source: CaudraId,
    target: CaudraId,
    ids: &[ToolOutputId],
) -> Result<(), SessionError> {
    let wanted = id_list(ids.iter())?;
    let copied = transaction.execute(
        "INSERT INTO saved_tool_outputs (session_id, output_id, created_at, byte_count, payload) \
         SELECT ?2, output_id, created_at, byte_count, payload FROM saved_tool_outputs \
         WHERE session_id = ?1 AND output_id IN (SELECT DISTINCT value FROM json_each(?3))",
        params![
            source.as_bytes().as_slice(),
            target.as_bytes().as_slice(),
            wanted
        ],
    )?;
    let distinct = ids.iter().collect::<HashSet<_>>().len();
    if copied == distinct {
        return Ok(());
    }
    let missing = ids
        .iter()
        .find(|id| {
            !transaction
                .query_row(
                    "SELECT 1 FROM saved_tool_outputs WHERE session_id = ?1 AND output_id = ?2",
                    params![target.as_bytes().as_slice(), id.as_str()],
                    |_| Ok(()),
                )
                .optional()
                .is_ok_and(|row| row.is_some())
        })
        .map_or_else(String::new, ToString::to_string);
    Err(StorageError::NotFound(format!("tool output {missing} of session {source}")).into())
}

/// Deletes every output of `session_id`, returning how many and their
/// uncompressed bytes.
pub(crate) fn delete_session_outputs(
    transaction: &Transaction<'_>,
    session_id: CaudraId,
) -> Result<(u64, u64), SessionError> {
    let (rows, bytes) = transaction.query_row(
        "SELECT count(*), coalesce(sum(byte_count), 0) FROM saved_tool_outputs \
         WHERE session_id = ?1",
        [session_id.as_bytes().as_slice()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    transaction.execute(
        "DELETE FROM saved_tool_outputs WHERE session_id = ?1",
        [session_id.as_bytes().as_slice()],
    )?;
    Ok((
        from_i64(rows, "trimmed saved outputs")?,
        from_i64(bytes, "trimmed saved output bytes")?,
    ))
}

/// `ids` as the JSON array SQL reads with `json_each`.
fn id_list<'a>(ids: impl Iterator<Item = &'a ToolOutputId>) -> Result<String, SessionError> {
    let ids = ids.map(ToolOutputId::as_str).collect::<Vec<_>>();
    Ok(serde_json::to_string(&ids).map_err(StorageError::from)?)
}

fn validate_page(offset: usize, limit: usize) -> Result<(), ToolOutputError> {
    if offset == 0 {
        return Err(ToolOutputError::InvalidOffset);
    }
    if limit == 0 {
        return Err(ToolOutputError::InvalidLimit);
    }
    Ok(())
}

pub fn line_count(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!text.is_empty() && !text.ends_with('\n'))
}

fn collect_references_from_archive(
    path: &Path,
    output_ids: &mut HashSet<ToolOutputId>,
) -> Option<()> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    for line in BufReader::new(File::open(path).ok()?).lines() {
        let line = line.ok()?;
        if !line.is_empty() {
            collect_output_ids(&serde_json::from_str(&line).ok()?, output_ids);
        }
    }
    Some(())
}

fn collect_output_ids(value: &serde_json::Value, output_ids: &mut HashSet<ToolOutputId>) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(id) = object
                .get("output_ref")
                .and_then(|reference| reference.get("id"))
                .and_then(serde_json::Value::as_str)
                .and_then(|id| id.parse().ok())
            {
                output_ids.insert(id);
            }
            if let Some(references) = object
                .get("retained_output_refs")
                .and_then(serde_json::Value::as_array)
            {
                output_ids.extend(references.iter().filter_map(|reference| {
                    reference
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| id.parse::<ToolOutputId>().ok())
                }));
            }
            for value in object.values() {
                collect_output_ids(value, output_ids);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_output_ids(value, output_ids);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, SystemTime};

    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::legacy::LEGACY_OUTPUT_DIR;
    use super::*;
    use crate::background::{TaskRecord, TranscriptWrite};
    use crate::sessions::{MAX_SQLITE_VALUE_BYTES, Session, SessionLease};

    const TEST_MAX_BYTES: usize = 1024 * 1024;
    const EXACT_TEXT: &str = "alpha\r\nβeta\n\nend";
    const PAGED_TEXT: &str = "one\ntwo\nthree\nfour\n";
    const STREAMED_TEXT: &str = "alpha\r\nβeta\n\nend\n";
    const MISSING_TEXT: &str = "missing";
    const KEPT_TEXT: &str = "another session's output";
    const READABLE_ID: &str = "brisk-calm-otter";
    const RETRY_ID: &str = "brisk-calm-otter-2";
    const NAMED_ID: &str = "output-cargo-test";
    const LEGACY_ID: &str = "CNK1hV6GWoysH3KQMm5wv";
    const MODEL: &str = "model";
    const CWD: &str = "/project";
    const LEGACY_MTIME_SECS: u64 = 1_000;
    const ROW_OVERHEAD_BYTES: usize = 1024;
    const LEGACY_CLEANUP_KIND: &str = "tool_output";
    const STALE: &str = "UPDATE saved_tool_outputs SET created_at = 0";
    const FOREIGN_KEY_IS_NOT_A_COLLISION: &str =
        "an output for a session without a row must fail as itself, not as an ID collision";
    const PRUNE_KEEPS_REFERENCED: &str = "an output a record still names must survive pruning";
    const IMPORT_SKIPS_UNUSABLE: &str = "a legacy file that cannot be imported must be left out";

    type TestSession = Session<Value, Value, Value>;

    fn test_store() -> (TempDir, ToolOutputStore, CaudraId) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let session_id = saved_session(&state_dir);
        (
            temp,
            ToolOutputStore::with_max_bytes(state_dir, TEST_MAX_BYTES),
            session_id,
        )
    }

    fn saved_session(state_dir: &StateDir) -> CaudraId {
        let mut session = TestSession::new(MODEL, CWD);
        session.save(state_dir).unwrap();
        session.id
    }

    fn database(store: &ToolOutputStore) -> SessionDatabase {
        SessionDatabase::open(&store.state_dir).unwrap()
    }

    fn row_count(store: &ToolOutputStore) -> u64 {
        database(store).stats().unwrap().saved_output_count
    }

    fn raw_insert(
        store: &ToolOutputStore,
        session_id: CaudraId,
        id: &str,
        byte_count: usize,
        payload: &[u8],
    ) {
        database(store)
            .connection()
            .execute(
                "INSERT INTO saved_tool_outputs (session_id, output_id, created_at, byte_count, payload) \
                 VALUES (?1, ?2, 0, ?3, ?4)",
                params![session_id.as_bytes().as_slice(), id, byte_count as i64, payload],
            )
            .unwrap();
    }

    fn not_found(result: Result<String, ToolOutputError>) -> bool {
        matches!(result, Err(ToolOutputError::NotFound { .. }))
    }

    #[test]
    fn the_largest_output_compresses_within_the_value_limit() {
        let bound = zstd::zstd_safe::compress_bound(DEFAULT_MAX_STORED_BYTES);

        assert!(bound + ROW_OVERHEAD_BYTES <= usize::try_from(MAX_SQLITE_VALUE_BYTES).unwrap());
    }

    #[test]
    fn put_saves_exact_utf8_text_compressed() {
        let (_temp, store, session_id) = test_store();
        let text = PAGED_TEXT.repeat(1_000);

        let reference = store.put(session_id, &text).unwrap();

        assert_eq!(reference.byte_count, text.len());
        assert_eq!(reference.line_count, line_count(&text));
        assert_eq!(
            store.load_text(session_id, reference.id.clone()).unwrap(),
            text
        );
        let stored: i64 = database(&store)
            .connection()
            .query_row(
                "SELECT length(payload) FROM saved_tool_outputs WHERE output_id = ?1",
                [reference.id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!((stored as usize) < text.len());
    }

    #[test]
    fn put_for_a_session_without_a_row_fails_as_a_database_error() {
        let (_temp, store, _) = test_store();

        let error = store.put(CaudraId::generate(), EXACT_TEXT).unwrap_err();

        assert!(
            matches!(error, ToolOutputError::Database(_)),
            "{FOREIGN_KEY_IS_NOT_A_COLLISION}: {error:?}"
        );
    }

    #[test]
    fn sink_saves_exact_appended_utf8_and_metadata() {
        let (_temp, store, session_id) = test_store();
        let mut sink = store.begin(session_id);

        sink.append("alpha\r\n").unwrap();
        sink.append("βeta").unwrap();
        sink.append("\n\nend\n").unwrap();
        assert_eq!(row_count(&store), 0);
        let reference = sink.finish().unwrap();

        assert_eq!(reference.byte_count, STREAMED_TEXT.len());
        assert_eq!(reference.line_count, line_count(STREAMED_TEXT));
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            STREAMED_TEXT
        );
    }

    #[test]
    fn sink_limit_rejects_append_but_can_save_accepted_prefix() {
        let (_temp, store, session_id) = test_store();
        let store = ToolOutputStore::with_max_bytes(store.state_dir, 5);
        let mut sink = store.begin(session_id);

        sink.append("αβ").unwrap();
        let error = sink.append("cd").unwrap_err();
        assert!(matches!(
            error,
            ToolOutputError::TooLarge {
                byte_count: 6,
                max_bytes: 5
            }
        ));

        let reference = sink.finish().unwrap();
        assert_eq!(reference.byte_count, 4);
        assert_eq!(store.load_text(session_id, reference.id).unwrap(), "αβ");
    }

    #[test]
    fn reserved_sink_append_leaves_room_for_control_but_generic_uses_full_cap() {
        const MAX_BYTES: usize = 10;
        const RESERVE_BYTES: usize = 4;
        let (_temp, store, session_id) = test_store();
        let store = ToolOutputStore::with_max_bytes(store.state_dir, MAX_BYTES);
        let mut sink = store.begin(session_id);

        sink.append_with_reserve("123456", RESERVE_BYTES).unwrap();
        assert!(matches!(
            sink.append_with_reserve("7", RESERVE_BYTES),
            Err(ToolOutputError::TooLarge {
                byte_count: 7,
                max_bytes: 6,
            })
        ));
        sink.append("7890").unwrap();

        let reference = sink.finish().unwrap();
        assert_eq!(reference.byte_count, MAX_BYTES);
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            "1234567890"
        );
    }

    #[test]
    fn a_dropped_sink_saves_nothing() {
        let (_temp, store, session_id) = test_store();
        let mut sink = store.begin(session_id);
        sink.append(EXACT_TEXT).unwrap();

        drop(sink);

        assert_eq!(row_count(&store), 0);
    }

    #[test_case(4, true ; "at_the_limit")]
    #[test_case(5, false ; "above_the_limit")]
    fn put_keeps_the_full_configured_limit(byte_count: usize, saved: bool) {
        let (_temp, store, session_id) = test_store();
        let store = ToolOutputStore::with_max_bytes(store.state_dir, 4);

        let result = store.put(session_id, &"1".repeat(byte_count));

        assert_eq!(result.is_ok(), saved);
        if !saved {
            assert!(matches!(result, Err(ToolOutputError::TooLarge { .. })));
        }
        assert_eq!(row_count(&store), u64::from(saved));
    }

    #[test]
    fn outputs_are_isolated_by_session() {
        let (_temp, store, owner) = test_store();
        let other = saved_session(&store.state_dir);
        let reference = store.put(owner, EXACT_TEXT).unwrap();

        assert_eq!(
            store.load_text(owner, reference.id.clone()).unwrap(),
            EXACT_TEXT
        );
        assert!(matches!(
            store.load_text(other, reference.id.clone()),
            Err(ToolOutputError::NotFound {
                session_id,
                output_id
            }) if session_id == other && output_id == reference.id
        ));
        assert!(matches!(
            store.read(other, reference.id, 1, 1),
            Err(ToolOutputError::NotFound { .. })
        ));
    }

    #[test_case(true; "saved_output")]
    #[test_case(false; "missing_output")]
    fn a_deleted_output_cannot_be_read_back(saved: bool) {
        let (_temp, store, session_id) = test_store();
        let id = if saved {
            store.put(session_id, EXACT_TEXT).unwrap().id
        } else {
            DEFAULT_OUTPUT_LABEL.parse().unwrap()
        };
        let other_session = saved_session(&store.state_dir);
        let kept = store.put(other_session, KEPT_TEXT).unwrap();

        store.delete(session_id, &id).unwrap();
        store.delete(session_id, &id).unwrap();

        assert!(not_found(store.load_text(session_id, id)));
        assert_eq!(store.load_text(other_session, kept.id).unwrap(), KEPT_TEXT);
    }

    #[test]
    fn read_and_grep_reject_a_payload_that_is_not_utf8() {
        let (_temp, store, session_id) = test_store();
        let bytes = b"visible\nhidden\xff";
        raw_insert(
            &store,
            session_id,
            READABLE_ID,
            bytes.len(),
            &zstd::bulk::compress(bytes, 3).unwrap(),
        );
        let id: ToolOutputId = READABLE_ID.parse().unwrap();

        assert!(matches!(
            store.read(session_id, id.clone(), 1, 1),
            Err(ToolOutputError::InvalidUtf8 { .. })
        ));
        assert!(matches!(
            store.grep(session_id, id, "visible", 1, 1, 0, 0),
            Err(ToolOutputError::InvalidUtf8 { .. })
        ));
    }

    #[test_case(b"not zstd", 8 ; "undecodable")]
    #[test_case(EXACT_TEXT.as_bytes(), EXACT_TEXT.len() + 1 ; "wrong_size")]
    fn a_damaged_payload_is_a_corrupt_value(raw: &[u8], byte_count: usize) {
        let (_temp, store, session_id) = test_store();
        let payload = if raw == EXACT_TEXT.as_bytes() {
            zstd::bulk::compress(raw, 3).unwrap()
        } else {
            raw.to_vec()
        };
        raw_insert(&store, session_id, READABLE_ID, byte_count, &payload);

        let error = store
            .load_text(session_id, READABLE_ID.parse().unwrap())
            .unwrap_err();

        assert!(matches!(
            error,
            ToolOutputError::Database(error)
                if matches!(*error, SessionError::CorruptDatabaseValue { .. })
        ));
    }

    #[test]
    fn a_row_above_the_limit_is_refused_before_decoding() {
        let (_temp, store, session_id) = test_store();
        raw_insert(&store, session_id, READABLE_ID, TEST_MAX_BYTES + 1, b"");

        assert!(matches!(
            store.load_text(session_id, READABLE_ID.parse().unwrap()),
            Err(ToolOutputError::TooLarge { .. })
        ));
    }

    #[test]
    fn grep_rejects_a_pattern_above_the_limit() {
        let (_temp, store, session_id) = test_store();
        let reference = store.put(session_id, EXACT_TEXT).unwrap();

        assert!(matches!(
            store.grep(
                session_id,
                reference.id,
                &"a".repeat(MAX_GREP_PATTERN_CHARS + 1),
                1,
                1,
                0,
                0
            ),
            Err(ToolOutputError::PatternTooLong { .. })
        ));
    }

    #[test_case(READABLE_ID; "readable")]
    #[test_case(LEGACY_ID; "legacy")]
    #[test_case("1111111111111111"; "legacy_leading_zeros")]
    #[test_case(MISSING_TEXT; "one_word")]
    #[test_case("brisk-calm"; "two_words")]
    #[test_case("brisk-calm-small-otter"; "four_words")]
    #[test_case("brisk-calm-otter1"; "digit")]
    #[test_case("output"; "generic")]
    #[test_case("output-cargo-test-4096"; "prefixed_suffixed")]
    #[test_case("42"; "numeric")]
    fn output_ids_round_trip_exact_canonical_text(raw: &str) {
        let id: ToolOutputId = raw.parse().unwrap();
        assert_eq!(id.as_str(), raw);
        assert_eq!(id.to_string(), raw);
        let json = serde_json::to_string(raw).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), json);
        assert_eq!(serde_json::from_str::<ToolOutputId>(&json).unwrap(), id);
    }

    #[test_case(""; "empty")]
    #[test_case("-calm-otter"; "empty_first")]
    #[test_case("brisk--otter"; "empty_middle")]
    #[test_case("brisk-calm-"; "empty_last")]
    #[test_case("Brisk-calm-otter"; "uppercase")]
    #[test_case("brisk-calm-otter "; "trailing_space")]
    #[test_case(" brisk-calm-otter"; "leading_space")]
    #[test_case("brisk-calm-otter\n"; "newline")]
    #[test_case("brisk\t-calm-otter"; "tab")]
    #[test_case("brisk-calm-ottér"; "unicode")]
    #[test_case("brisk-calm-otter\0"; "nul")]
    #[test_case("../brisk-calm-otter"; "parent_path")]
    #[test_case("brisk-calm-.."; "parent_component")]
    #[test_case("/brisk-calm-otter"; "absolute_path")]
    #[test_case("brisk/calm/otter"; "slash")]
    #[test_case("brisk\\calm\\otter"; "backslash")]
    #[test_case("brisk-calm-otter.txt"; "extension")]
    #[test_case(" CNK1hV6GWoysH3KQMm5wv"; "legacy_space")]
    #[test_case("1CNK1hV6GWoysH3KQMm5wv"; "legacy_extra_zero")]
    fn output_ids_reject_invalid_parse_and_deserialization(raw: &str) {
        assert_eq!(raw.parse::<ToolOutputId>(), Err(ToolOutputIdParseError));
        assert!(serde_json::from_value::<ToolOutputId>(Value::String(raw.into())).is_err());
    }

    #[test_case(0, true; "at_bound")]
    #[test_case(1, false; "above_bound")]
    fn output_ids_enforce_length_bound(extra: usize, valid: bool) {
        let raw = format!("{}-b-c", "a".repeat(DESCRIPTIVE_ID_MAX_LEN - 4 + extra));
        assert_eq!(raw.parse::<ToolOutputId>().is_ok(), valid);
        assert_eq!(
            serde_json::from_value::<ToolOutputId>(Value::String(raw)).is_ok(),
            valid
        );
    }

    #[test_case("Output Cargo Test", NAMED_ID; "normalized")]
    #[test_case("cargo-test", "cargo-test"; "no_automatic_prefix")]
    #[test_case("東京", DEFAULT_OUTPUT_LABEL; "fallback")]
    fn new_outputs_use_descriptive_ids(label: &str, expected: &str) {
        let (_temp, store, session_id) = test_store();
        let reference = store.put_named(session_id, EXACT_TEXT, label).unwrap();
        let second = store.put_named(session_id, PAGED_TEXT, expected).unwrap();
        assert_eq!(reference.id.as_str(), expected);
        assert_eq!(second.id.as_str(), format!("{expected}-2"));
        assert_eq!(store.load_text(session_id, second.id).unwrap(), PAGED_TEXT);
        assert_eq!(
            store.load_text(session_id, reference.id).unwrap(),
            EXACT_TEXT
        );
    }

    #[test]
    fn generic_wrappers_share_the_output_label() {
        let (_temp, store, session_id) = test_store();
        let first = store.put(session_id, EXACT_TEXT).unwrap();
        let second = store.begin(session_id).finish().unwrap();
        assert_eq!(first.id.as_str(), DEFAULT_OUTPUT_LABEL);
        assert_eq!(second.id.as_str(), format!("{DEFAULT_OUTPUT_LABEL}-2"));
    }

    #[test_case(false; "creation_order")]
    #[test_case(true; "reverse_order")]
    fn named_sinks_allocate_in_save_order(reverse: bool) {
        let (_temp, store, session_id) = test_store();
        let mut first = store.begin_named(session_id, NAMED_ID);
        let mut second = store.begin_named(session_id, NAMED_ID);
        first.append(EXACT_TEXT).unwrap();
        second.append(PAGED_TEXT).unwrap();
        let (first, second, first_text, second_text) = if reverse {
            (second, first, PAGED_TEXT, EXACT_TEXT)
        } else {
            (first, second, EXACT_TEXT, PAGED_TEXT)
        };
        let first = first.finish().unwrap();
        let second = second.finish().unwrap();
        assert_eq!(first.id.as_str(), NAMED_ID);
        assert_eq!(second.id.as_str(), format!("{NAMED_ID}-2"));
        assert_eq!(store.load_text(session_id, first.id).unwrap(), first_text);
        assert_eq!(store.load_text(session_id, second.id).unwrap(), second_text);
    }

    #[test]
    fn named_outputs_choose_the_first_available_gap() {
        let (_temp, store, session_id) = test_store();
        let first = store.put_named(session_id, EXACT_TEXT, NAMED_ID).unwrap();
        let gap = store.put_named(session_id, PAGED_TEXT, NAMED_ID).unwrap();
        let third = store
            .put_named(session_id, STREAMED_TEXT, NAMED_ID)
            .unwrap();
        store.delete(session_id, &gap.id).unwrap();

        let replacement = store.put_named(session_id, KEPT_TEXT, NAMED_ID).unwrap();

        assert_eq!(replacement.id, gap.id);
        assert_eq!(
            store.load_text(session_id, replacement.id).unwrap(),
            KEPT_TEXT
        );
        assert_eq!(store.load_text(session_id, first.id).unwrap(), EXACT_TEXT);
        assert_eq!(
            store.load_text(session_id, third.id).unwrap(),
            STREAMED_TEXT
        );
    }

    #[test]
    fn exhausted_candidates_are_an_id_collision_that_keeps_every_output() {
        let (_temp, store, session_id) = test_store();
        let winner = store
            .put_named(session_id, EXACT_TEXT, READABLE_ID)
            .unwrap();
        let mut database = database(&store);
        let transaction = database.connection_mut().transaction().unwrap();
        for candidate in DescriptiveIdCandidates::new(READABLE_ID, DEFAULT_OUTPUT_LABEL).skip(1) {
            transaction
                .execute(
                    "INSERT INTO saved_tool_outputs \
                         (session_id, output_id, created_at, byte_count, payload) \
                     VALUES (?1, ?2, 0, 0, x'')",
                    params![session_id.as_bytes().as_slice(), candidate],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(database);
        let mut loser = store.begin_named(session_id, READABLE_ID);
        loser.append(PAGED_TEXT).unwrap();

        assert!(matches!(loser.finish(), Err(ToolOutputError::IdCollision)));
        assert_eq!(row_count(&store), DESCRIPTIVE_ID_ATTEMPTS as u64);
        assert_eq!(store.load_text(session_id, winner.id).unwrap(), EXACT_TEXT);
    }

    #[test]
    fn concurrent_sinks_take_distinct_ids_without_overwriting() {
        let (_temp, store, session_id) = test_store();
        let barrier = Barrier::new(2);
        let outputs = thread::scope(|scope| {
            let handles: Vec<_> = [EXACT_TEXT, PAGED_TEXT]
                .into_iter()
                .map(|text| {
                    let barrier = &barrier;
                    let store = ToolOutputStore::new(store.state_dir.clone());
                    scope.spawn(move || {
                        let mut sink = store.begin_named(session_id, READABLE_ID);
                        sink.append(text).unwrap();
                        barrier.wait();
                        let reference = sink.finish().unwrap();
                        assert_eq!(
                            store.load_text(session_id, reference.id.clone()).unwrap(),
                            text
                        );
                        reference.id
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<HashSet<_>>()
        });

        assert_eq!(
            outputs
                .iter()
                .map(ToolOutputId::as_str)
                .collect::<HashSet<_>>(),
            HashSet::from([READABLE_ID, RETRY_ID])
        );
        assert_eq!(row_count(&store), 2);
    }

    #[test]
    fn prune_keeps_referenced_outputs_and_young_ones() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::new(state_dir.clone());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&state_dir).unwrap();
        let in_history = store.put(session.id, EXACT_TEXT).unwrap();
        let in_subagent = store.put(session.id, PAGED_TEXT).unwrap();
        let in_archive = store.put(session.id, STREAMED_TEXT).unwrap();
        let unreferenced = store.put(session.id, MISSING_TEXT).unwrap();
        session.push_message(json!({"output_ref": in_archive}));
        session.push_message(json!({"old": true}));
        session.save(&state_dir).unwrap();
        session.replace_messages(vec![json!({"output_ref": in_history})]);
        session.set_subagent_messages(
            "task-1".into(),
            vec![json!({"retained_output_refs": [in_subagent]})],
        );
        session.save(&state_dir).unwrap();
        let database = SessionDatabase::open(&state_dir).unwrap();

        assert_eq!(
            database
                .prune_unreferenced_outputs(UNREFERENCED_OUTPUT_GRACE, false)
                .unwrap(),
            0
        );
        database.connection().execute(STALE, []).unwrap();
        assert_eq!(store.prune_unreferenced(true).unwrap(), 1);
        assert_eq!(store.prune_unreferenced(false).unwrap(), 1);

        for kept in [in_history, in_subagent, in_archive] {
            assert!(
                store.load_text(session.id, kept.id).is_ok(),
                "{PRUNE_KEEPS_REFERENCED}"
            );
        }
        assert!(not_found(store.load_text(session.id, unreferenced.id)));
    }

    #[test_case(false; "outcome")]
    #[test_case(true; "transcript")]
    fn prune_keeps_outputs_referenced_only_by_a_background_task(in_transcript: bool) {
        let (_temp, store, session_id) = test_store();
        let referenced = store.put(session_id, EXACT_TEXT).unwrap();
        let unreferenced = store.put(session_id, MISSING_TEXT).unwrap();
        let mut record = json!({
            "created_at": 0,
            "updated_at": 0,
            "sequence": 1,
            "task_id": "task-1",
            "invocation_id": "invocation-1",
            "root_call_id": "call-1",
            "generation": 1,
            "state": "succeeded",
            "background": true,
            "receipt_accepted": true,
            "mode": "build",
            "request": {},
            "outcome": {"success": true},
            "events": [],
        });
        let transcript = json!([{"role": "user", "output_ref": referenced.clone()}]).to_string();
        let transcript = if in_transcript {
            TranscriptWrite::Save(transcript)
        } else {
            record["output_ref"] = json!(referenced);
            TranscriptWrite::Keep
        };
        let record: TaskRecord = serde_json::from_value(record).unwrap();
        let database = database(&store);
        database
            .save_background_task_runtime(session_id, &record, &transcript, &runtime_retry())
            .unwrap();
        database.connection().execute(STALE, []).unwrap();

        assert_eq!(store.prune_unreferenced(false).unwrap(), 1);
        assert!(
            store.load_text(session_id, referenced.id).is_ok(),
            "{PRUNE_KEEPS_REFERENCED}"
        );
        assert!(not_found(store.load_text(session_id, unreferenced.id)));
    }

    #[test]
    fn deleting_a_session_deletes_its_outputs() {
        let (_temp, store, session_id) = test_store();
        let other = saved_session(&store.state_dir);
        let reference = store.put(session_id, EXACT_TEXT).unwrap();
        let kept = store.put(other, KEPT_TEXT).unwrap();

        database(&store).delete(session_id, None).unwrap();

        assert!(not_found(store.load_text(session_id, reference.id)));
        assert_eq!(store.load_text(other, kept.id).unwrap(), KEPT_TEXT);
    }

    #[test]
    fn trimming_a_session_deletes_and_reports_its_outputs() {
        let (_temp, store, session_id) = test_store();
        let reference = store.put(session_id, EXACT_TEXT).unwrap();
        store.put(session_id, PAGED_TEXT).unwrap();
        let lease = SessionLease::acquire(&store.state_dir, session_id).unwrap();

        let report = database(&store).trim(&lease).unwrap();

        assert_eq!(report.saved_output_rows, 2);
        assert_eq!(
            report.saved_output_bytes,
            (EXACT_TEXT.len() + PAGED_TEXT.len()) as u64
        );
        assert!(not_found(store.load_text(session_id, reference.id)));
    }

    #[test]
    fn a_fork_copies_each_named_output_once_in_its_first_save() {
        let (_temp, store, source) = test_store();
        let reference = store.put(source, EXACT_TEXT).unwrap();
        store.put(source, PAGED_TEXT).unwrap();
        let mut fork = TestSession::new(MODEL, CWD);
        fork.copy_saved_outputs_from(source, vec![reference.id.clone(), reference.id.clone()]);

        fork.save(&store.state_dir).unwrap();
        fork.save(&store.state_dir).unwrap();

        assert_eq!(store.load_text(fork.id, reference.id).unwrap(), EXACT_TEXT);
        assert_eq!(row_count(&store), 3);
    }

    #[test]
    fn a_fork_missing_an_output_is_not_saved() {
        let (_temp, store, source) = test_store();
        let kept = store.put(source, EXACT_TEXT).unwrap();
        let mut fork = TestSession::new(MODEL, CWD);
        fork.copy_saved_outputs_from(source, vec![kept.id.clone(), MISSING_TEXT.parse().unwrap()]);

        let error = fork.save(&store.state_dir).unwrap_err();

        assert!(error.to_string().contains(MISSING_TEXT), "{error}");
        assert!(database(&store).persisted_session_ids().unwrap() == [source]);
        assert_eq!(row_count(&store), 1);
    }

    fn legacy_file(state_dir: &StateDir, session: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let directory = state_dir.path().join(LEGACY_OUTPUT_DIR).join(session);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn migration_imports_legacy_files_and_retires_their_directory() {
        let (_temp, store, live) = test_store();
        let state_dir = store.state_dir.clone();
        let trimmed = saved_session(&state_dir);
        let database = database(&store);
        database
            .connection()
            .execute_batch(&format!(
                "ALTER TABLE shell_runs DROP COLUMN host; \
                 DROP TABLE {SAVED_OUTPUTS_TABLE}; PRAGMA user_version = 40;"
            ))
            .unwrap();
        database
            .connection()
            .execute(
                "INSERT INTO cleanup_jobs (session_id, kind, next_attempt_ms) VALUES (?1, ?2, ?3)",
                params![trimmed.as_bytes().as_slice(), LEGACY_CLEANUP_KIND, i64::MAX],
            )
            .unwrap();
        drop(database);
        let live_name = live.to_string();
        let readable = legacy_file(
            &state_dir,
            &live_name,
            &format!("{READABLE_ID}.txt"),
            PAGED_TEXT.as_bytes(),
        );
        File::options()
            .write(true)
            .open(&readable)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(LEGACY_MTIME_SECS))
            .unwrap();
        legacy_file(
            &state_dir,
            &live_name,
            &format!("{LEGACY_ID}.txt"),
            EXACT_TEXT.as_bytes(),
        );
        legacy_file(&state_dir, &live_name, &format!("{NAMED_ID}.txt"), b"\xff");
        legacy_file(
            &state_dir,
            &live_name,
            "Not An Id.txt",
            EXACT_TEXT.as_bytes(),
        );
        legacy_file(&state_dir, &live_name, ".tmp-stage", EXACT_TEXT.as_bytes());
        File::options()
            .write(true)
            .open(legacy_file(
                &state_dir,
                &live_name,
                &format!("{MISSING_TEXT}.txt"),
                b"",
            ))
            .unwrap()
            .set_len(DEFAULT_MAX_STORED_BYTES as u64 + 1)
            .unwrap();
        #[cfg(unix)]
        symlink(
            &readable,
            readable.with_file_name(format!("{RETRY_ID}.txt")),
        )
        .unwrap();
        legacy_file(
            &state_dir,
            &trimmed.to_string(),
            &format!("{READABLE_ID}.txt"),
            EXACT_TEXT.as_bytes(),
        );
        legacy_file(
            &state_dir,
            &CaudraId::generate().to_string(),
            &format!("{READABLE_ID}.txt"),
            EXACT_TEXT.as_bytes(),
        );
        legacy_file(
            &state_dir,
            "not-a-session",
            &format!("{READABLE_ID}.txt"),
            EXACT_TEXT.as_bytes(),
        );

        let migrated = SessionDatabase::open(&state_dir).unwrap();

        assert_eq!(
            store.load_text(live, READABLE_ID.parse().unwrap()).unwrap(),
            PAGED_TEXT
        );
        assert_eq!(
            store.load_text(live, LEGACY_ID.parse().unwrap()).unwrap(),
            EXACT_TEXT
        );
        assert_eq!(
            migrated.stats().unwrap().saved_output_count,
            2,
            "{IMPORT_SKIPS_UNUSABLE}"
        );
        let created_at: i64 = migrated
            .connection()
            .query_row(
                "SELECT created_at FROM saved_tool_outputs WHERE output_id = ?1",
                [READABLE_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created_at, LEGACY_MTIME_SECS as i64);
        let legacy_jobs: i64 = migrated
            .connection()
            .query_row(
                "SELECT count(*) FROM cleanup_jobs WHERE kind = ?1",
                [LEGACY_CLEANUP_KIND],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_jobs, 0);
        assert!(!state_dir.path().join(LEGACY_OUTPUT_DIR).exists());
    }

    #[test]
    fn opening_a_current_database_retires_a_leftover_legacy_directory() {
        let (_temp, store, session_id) = test_store();
        legacy_file(
            &store.state_dir,
            &session_id.to_string(),
            &format!("{READABLE_ID}.txt"),
            EXACT_TEXT.as_bytes(),
        );

        drop(database(&store));

        assert!(!store.state_dir.path().join(LEGACY_OUTPUT_DIR).exists());
    }
}
