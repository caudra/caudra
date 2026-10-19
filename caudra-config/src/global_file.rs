//! Edits to the global `caudra.toml` made from inside Caudra: read with the revision they commit
//! against, checked to still load, and written atomically, so a concurrent edit is never lost.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf, absolute};
use std::sync::{Mutex, PoisonError};

use caudra_storage::private_file::FileRevision;
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::DocumentMut;
use tracing::warn;

use crate::config_file::GlobalConfigFile;
use crate::interpolation::Lookup;
use crate::own_writes;

const STAGING_PREFIX: &str = ".caudra.toml.";
const NOT_UTF8: &str = "the file is not valid UTF-8";

/// Serializes this process's commits, so two edits from one process cannot both pass the
/// revision check.
static COMMITS: Mutex<()> = Mutex::new(());

#[derive(Debug, Error)]
pub enum GlobalFileError {
    #[error("{} changed since it was loaded; reload it and make the change again", path.display())]
    Conflict { path: PathBuf },
    #[error("the change would leave {} invalid: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
    #[error("cannot update {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} does not load, so it cannot be edited here: {message}", path.display())]
    Unparseable { path: PathBuf, message: String },
}

/// The global `caudra.toml` as written, `{env:NAME}` and `{file:PATH}` references unexpanded.
/// A missing file is an empty one.
#[derive(Debug, Clone)]
pub(crate) struct GlobalDocument {
    pub(crate) path: PathBuf,
    pub(crate) revision: FileRevision,
    pub(crate) document: DocumentMut,
}

impl GlobalDocument {
    pub(crate) fn load(path: &Path) -> Result<Self, GlobalFileError> {
        let bytes = read(path).map_err(|source| GlobalFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let unparseable = |message: String| GlobalFileError::Unparseable {
            path: path.to_path_buf(),
            message,
        };
        let revision = revision_of(bytes.as_deref());
        let text = bytes
            .map(String::from_utf8)
            .transpose()
            .map_err(|_| unparseable(NOT_UTF8.into()))?
            .unwrap_or_default();
        let document = validated(path, &text).map_err(unparseable)?;
        Ok(Self {
            path: path.to_path_buf(),
            revision,
            document,
        })
    }

    /// Writes `document` only while the file still holds what [`Self::load`] read, and only
    /// when it loads as a valid global config.
    pub(crate) fn commit(&self, document: &DocumentMut) -> Result<Self, GlobalFileError> {
        let text = document.to_string();
        let document =
            validated(&self.path, &text).map_err(|message| GlobalFileError::Invalid {
                path: self.path.clone(),
                message,
            })?;
        let revision = commit(&self.path, &self.revision, text.as_bytes())?;
        own_writes::record(&self.path, text.as_bytes());
        Ok(Self {
            path: self.path.clone(),
            revision,
            document,
        })
    }
}

/// `text` as an editable document, once it loads as a valid global config.
fn validated(path: &Path, text: &str) -> Result<DocumentMut, String> {
    GlobalConfigFile::parse(text, Lookup::Global(path))
        .map_err(|error| error.to_string())?
        .settings
        .into_config(false)
        .map_err(|error| error.to_string())?;
    text.parse().map_err(|error| format!("{error}"))
}

fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn revision_of(bytes: Option<&[u8]>) -> FileRevision {
    bytes.map_or(FileRevision::Missing, |bytes| {
        FileRevision::Present(Sha256::digest(bytes).into())
    })
}

/// Writes through a symlink to the file it names, so a linked dotfile stays linked. The new
/// file replaces the old one atomically and keeps its permissions; a new file is owner-only.
fn commit(
    path: &Path,
    expected: &FileRevision,
    contents: &[u8],
) -> Result<FileRevision, GlobalFileError> {
    let _serialized = COMMITS.lock().unwrap_or_else(PoisonError::into_inner);
    let failed = |source: io::Error| GlobalFileError::Io {
        path: path.to_path_buf(),
        source,
    };
    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => absolute(path).map_err(failed)?,
        Err(error) => return Err(failed(error)),
    };
    let current = read(&target).map_err(failed)?;
    if revision_of(current.as_deref()) != *expected {
        return Err(GlobalFileError::Conflict {
            path: path.to_path_buf(),
        });
    }
    if current.as_deref().unwrap_or_default() == contents {
        return Ok(expected.clone());
    }
    let directory = target
        .parent()
        .ok_or_else(|| failed(io::ErrorKind::InvalidInput.into()))?;
    fs::create_dir_all(directory).map_err(failed)?;
    let mut staged = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempfile_in(directory)
        .map_err(failed)?;
    staged.write_all(contents).map_err(failed)?;
    if current.is_some() {
        let permissions = fs::metadata(&target).map_err(failed)?.permissions();
        staged
            .as_file()
            .set_permissions(permissions)
            .map_err(failed)?;
    }
    staged.as_file().sync_all().map_err(failed)?;
    staged
        .persist(&target)
        .map_err(|error| failed(error.error))?;
    if let Err(error) = File::open(directory).and_then(|directory| directory.sync_all()) {
        warn!(path = %target.display(), %error, "config directory sync failed after an edit");
    }
    Ok(revision_of(Some(contents)))
}
