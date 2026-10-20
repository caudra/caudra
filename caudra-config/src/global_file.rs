//! Edits to the global `caudra.toml` made from inside Caudra: read with the revision they commit
//! against, checked to still load, and written atomically, so a concurrent edit is never lost.

use std::path::{Path, PathBuf};

use caudra_storage::private_file::FileRevision;
use thiserror::Error;
use toml_edit::DocumentMut;

use crate::atomic_file::{self, AtomicFileError};
use crate::config_file::{self, CONFIG_FORMAT};
use crate::migration::{self, ConfigFormat, MigrationError};

const NOT_UTF8: &str = "the file is not valid UTF-8";

#[derive(Debug, Error)]
pub enum GlobalFileError {
    #[error(transparent)]
    File(#[from] AtomicFileError),
    #[error("the change would leave {} invalid: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
    #[error("{} does not load, so it cannot be edited here: {message}", path.display())]
    Unparseable { path: PathBuf, message: String },
    #[error(transparent)]
    Format(#[from] MigrationError),
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
        let bytes = atomic_file::read(path)?;
        let unparseable = |message: String| GlobalFileError::Unparseable {
            path: path.to_path_buf(),
            message,
        };
        let revision = atomic_file::revision_of(bytes.as_deref());
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

    /// Writes `document` only while the file still holds what [`Self::load`] read, only
    /// when it loads as a valid global config, and never over a file still waiting for its
    /// format upgrade.
    pub(crate) fn commit(&self, document: &DocumentMut) -> Result<Self, GlobalFileError> {
        self.commit_in(document, &CONFIG_FORMAT)
    }

    fn commit_in(
        &self,
        document: &DocumentMut,
        format: &ConfigFormat,
    ) -> Result<Self, GlobalFileError> {
        migration::ensure_current(&self.document, format)?;
        let text = document.to_string();
        let document =
            validated(&self.path, &text).map_err(|message| GlobalFileError::Invalid {
                path: self.path.clone(),
                message,
            })?;
        let revision = atomic_file::commit(&self.path, &self.revision, text.as_bytes())?;
        Ok(Self {
            path: self.path.clone(),
            revision,
            document,
        })
    }
}

/// `text` as an editable document, once it loads as a valid global config. The document stays
/// as written; only the check reads it upgraded.
fn validated(path: &Path, text: &str) -> Result<DocumentMut, String> {
    let loaded = migration::upgrade_text(text.to_owned(), &CONFIG_FORMAT)
        .map_err(|error| error.to_string())?;
    config_file::validate_global(path, &loaded.text)?;
    text.parse().map_err(|error| format!("{error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use toml_edit::DocumentMut;

    use super::{GlobalDocument, GlobalFileError};
    use crate::config_file::CONFIG_FILE;
    use crate::migration::MigrationError;
    use crate::migration::tests_support::BEHIND;

    const ON_DISK: &str = "version = 1\nalways_yolo = false\n";
    const EDITED: &str = "version = 1\nalways_yolo = true\n";

    #[test]
    fn a_commit_over_a_file_behind_its_format_is_refused() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        fs::write(&path, ON_DISK).unwrap();
        let loaded = GlobalDocument::load(&path).unwrap();
        let edited: DocumentMut = EDITED.parse().unwrap();

        let error = loaded.commit_in(&edited, &BEHIND).unwrap_err();

        assert!(
            matches!(
                error,
                GlobalFileError::Format(MigrationError::Pending { .. })
            ),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), ON_DISK);
        loaded.commit(&edited).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), EDITED);
    }
}
