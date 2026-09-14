mod atomic_write;
mod edit;
mod patch;
mod patch_publication;
mod write;

use std::{
    mem::size_of,
    path::{Path, PathBuf},
};

use crate::FilesystemError;
use crate::text::FileVersion;
use crate::types::{FileDiff, FileMutationType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlannedChangeType {
    Add,
    Update,
    Delete,
    Move,
}

impl From<PlannedChangeType> for FileMutationType {
    fn from(value: PlannedChangeType) -> Self {
        match value {
            PlannedChangeType::Add => Self::Add,
            PlannedChangeType::Update => Self::Update,
            PlannedChangeType::Delete => Self::Delete,
            PlannedChangeType::Move => Self::Move,
        }
    }
}

pub(crate) struct PlannedChange {
    pub(crate) requested_path: String,
    pub(crate) file_path: PathBuf,
    pub(crate) new_content: String,
    pub(crate) change_type: PlannedChangeType,
    pub(crate) move_path: Option<PathBuf>,
    pub(crate) requested_move_path: Option<String>,
    pub(crate) diff: FileDiff,
    pub(crate) expected_source: Option<FileVersion>,
}

impl PlannedChange {
    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.requested_path.capacity())
            .saturating_add(self.file_path.capacity())
            .saturating_add(self.new_content.capacity())
            .saturating_add(self.move_path.as_ref().map_or(0, PathBuf::capacity))
            .saturating_add(
                self.requested_move_path
                    .as_ref()
                    .map_or(0, String::capacity),
            )
            .saturating_add(self.diff.retained_bytes())
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn publication_stale(requested_path: &str) -> FilesystemError {
    FilesystemError::message(format!(
        "Prepared resource changed before publication: {requested_path}"
    ))
}
