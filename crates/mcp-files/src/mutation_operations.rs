mod atomic_write;
mod edit;
mod patch;
mod patch_publication;
mod write;

use std::path::{Path, PathBuf};

use crate::text::FileVersion;
use crate::types::{FileDiff, FileMutationType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlannedChangeType {
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

struct PlannedChange {
    file_path: PathBuf,
    new_content: String,
    change_type: PlannedChangeType,
    move_path: Option<PathBuf>,
    diff: FileDiff,
    expected_source: Option<FileVersion>,
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
