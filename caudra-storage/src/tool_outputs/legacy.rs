//! The one-time move of the `tool-output/<session>/<id>.txt` files that held
//! saved outputs before schema 41 into `saved_tool_outputs`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Transaction, params};
use tracing::{info, warn};

use super::{
    DEFAULT_MAX_STORED_BYTES, SAVED_OUTPUTS_TABLE, ToolOutputId, compress_payload, to_i64,
};
use crate::id::CaudraId;
use crate::sessions::SessionError;
use crate::sessions::progress::{MIGRATION, MigrationEvent};
use crate::{StateDir, StorageError};

pub(crate) const LEGACY_OUTPUT_DIR: &str = "tool-output";
const LEGACY_EXTENSION: &str = ".txt";
const LEGACY_CLEANUP_KIND: &str = "tool_output";
const SKIP_NO_SESSION: &str = "no_session";
const SKIP_TRIMMED: &str = "trimmed";
const SKIP_NOT_REGULAR: &str = "not_regular";
const SKIP_BAD_NAME: &str = "bad_name";
const SKIP_OVERSIZED: &str = "oversized";
const SKIP_NOT_UTF8: &str = "not_utf8";
const SKIP_UNREADABLE: &str = "unreadable";

struct LegacyOutput {
    session_id: CaudraId,
    output_id: ToolOutputId,
    path: PathBuf,
}

/// Imports every legacy output whose session still has a row, inside the
/// migration's transaction, and retires the cleanup jobs that deleted them.
/// A file that cannot be imported is skipped and counted, never fatal; a
/// directory that cannot be listed fails the migration, because the files
/// are removed once it commits.
pub(crate) fn import_legacy_outputs(
    transaction: &Transaction<'_>,
    state_dir: &StateDir,
) -> Result<(), SessionError> {
    let mut skipped = BTreeMap::new();
    let outputs = legacy_outputs(transaction, state_dir, &mut skipped)?;
    let total = outputs.len() as u64;
    let report = |done| {
        MIGRATION.report(MigrationEvent::Rewrite {
            table: SAVED_OUTPUTS_TABLE,
            done,
            total,
        });
    };
    report(0);
    let mut insert = transaction.prepare(
        "INSERT INTO saved_tool_outputs (session_id, output_id, created_at, byte_count, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut imported = 0;
    for (done, output) in (1..).zip(outputs) {
        match read_legacy(&output.path) {
            Ok((text, created_at)) => {
                insert.execute(params![
                    output.session_id.as_bytes().as_slice(),
                    output.output_id.as_str(),
                    created_at,
                    to_i64(text.len(), "legacy tool output bytes")?,
                    compress_payload(&text)?
                ])?;
                imported += 1;
            }
            Err(reason) => *skipped.entry(reason).or_insert(0u64) += 1,
        }
        report(done);
    }
    transaction.execute(
        "DELETE FROM cleanup_jobs WHERE kind = ?1",
        [LEGACY_CLEANUP_KIND],
    )?;
    if imported > 0 || !skipped.is_empty() {
        info!(imported, ?skipped, "imported legacy tool output files");
    }
    Ok(())
}

/// Removes the legacy directory once its outputs live in the database. Runs
/// on every repository open, so an interrupted removal finishes later.
pub(crate) fn retire_legacy_outputs(state_dir: &StateDir) -> std::io::Result<()> {
    let root = state_dir.path().join(LEGACY_OUTPUT_DIR);
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&root),
        Ok(_) => fs::remove_file(&root),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn legacy_outputs(
    transaction: &Transaction<'_>,
    state_dir: &StateDir,
    skipped: &mut BTreeMap<&'static str, u64>,
) -> Result<Vec<LegacyOutput>, SessionError> {
    let root = state_dir.path().join(LEGACY_OUTPUT_DIR);
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            warn!(path = %root.display(), "legacy tool output root is not a directory; skipped");
            return Ok(Vec::new());
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(StorageError::from(error).into()),
    }
    let mut owned = transaction.prepare(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1), \
                EXISTS(SELECT 1 FROM cleanup_jobs WHERE session_id = ?1 AND kind = ?2)",
    )?;
    let mut outputs = Vec::new();
    for entry in list(&root)? {
        let name = entry.file_name();
        let Some(session_id) = name.to_str().and_then(|name| {
            name.parse::<CaudraId>()
                .ok()
                .filter(|id| id.to_string() == name)
        }) else {
            *skipped.entry(SKIP_BAD_NAME).or_insert(0) += 1;
            continue;
        };
        if !entry.file_type().map_err(StorageError::from)?.is_dir() {
            *skipped.entry(SKIP_NOT_REGULAR).or_insert(0) += 1;
            continue;
        }
        let (exists, trimmed) = owned.query_row(
            params![session_id.as_bytes().as_slice(), LEGACY_CLEANUP_KIND],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
        )?;
        let files = list(&entry.path())?;
        let reason = match (exists, trimmed) {
            (false, _) => Some(SKIP_NO_SESSION),
            (true, true) => Some(SKIP_TRIMMED),
            (true, false) => None,
        };
        if let Some(reason) = reason {
            *skipped.entry(reason).or_insert(0) += files.len() as u64;
            continue;
        }
        for file in files {
            let output_id = file.file_name().to_str().and_then(|name| {
                let stem = name.strip_suffix(LEGACY_EXTENSION)?;
                stem.parse::<ToolOutputId>()
                    .ok()
                    .filter(|id| id.as_str() == stem)
            });
            match (output_id, file.file_type()) {
                (Some(output_id), Ok(file_type)) if file_type.is_file() => {
                    outputs.push(LegacyOutput {
                        session_id,
                        output_id,
                        path: file.path(),
                    });
                }
                (None, _) => *skipped.entry(SKIP_BAD_NAME).or_insert(0) += 1,
                _ => *skipped.entry(SKIP_NOT_REGULAR).or_insert(0) += 1,
            }
        }
    }
    Ok(outputs)
}

fn list(directory: &Path) -> Result<Vec<fs::DirEntry>, SessionError> {
    fs::read_dir(directory)
        .and_then(Iterator::collect)
        .map_err(|error| StorageError::from(error).into())
}

/// The text of one legacy file and when it was written, or why it is skipped.
fn read_legacy(path: &Path) -> Result<(String, i64), &'static str> {
    let file = open_no_follow(path).map_err(|_| SKIP_UNREADABLE)?;
    let metadata = file.metadata().map_err(|_| SKIP_UNREADABLE)?;
    if !metadata.is_file() {
        return Err(SKIP_NOT_REGULAR);
    }
    if metadata.len() > DEFAULT_MAX_STORED_BYTES as u64 {
        return Err(SKIP_OVERSIZED);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(DEFAULT_MAX_STORED_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SKIP_UNREADABLE)?;
    if bytes.len() > DEFAULT_MAX_STORED_BYTES {
        return Err(SKIP_OVERSIZED);
    }
    let text = String::from_utf8(bytes).map_err(|_| SKIP_NOT_UTF8)?;
    let created_at = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |age| i64::try_from(age.as_secs()).unwrap_or(i64::MAX));
    Ok((text, created_at))
}

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    options.open(path)
}
