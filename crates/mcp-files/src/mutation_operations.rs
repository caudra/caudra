mod atomic_write;
mod edit;
mod patch;
mod patch_publication;
mod write;

use std::path::{Path, PathBuf};

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
    pub(crate) file_path: PathBuf,
    pub(crate) new_content: String,
    pub(crate) change_type: PlannedChangeType,
    pub(crate) move_path: Option<PathBuf>,
    pub(crate) diff: FileDiff,
    pub(crate) expected_source: Option<FileVersion>,
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
