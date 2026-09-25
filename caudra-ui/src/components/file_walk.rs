//! The project walk that feeds path pickers.
//!
//! Shared by the file-picker modal and the `@` mention popup so a project is
//! never listed by two different sets of ignore rules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use caudra_workbench::{ResourceEntry, WorkbenchPath};
use caudra_workspace::ResourceKind;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use nucleo::{Injector, Utf32String};
use tracing::warn;

pub(crate) const UNREADABLE_DIR_MSG: &str = "Cannot list the current directory";
pub(crate) const NOTHING_TO_PICK_MSG: &str = "Nothing to pick in the current directory";
const WALKER_CRASHED_MSG: &str = "file walker crashed";
const THREAD_NAME: &str = "file-walker";
const GIT_OVERRIDE: &str = "!.git";
const REMOTE_SEPARATOR: char = '/';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Walk {
    Running,
    Listed,
    Unreadable,
}

impl Walk {
    /// What to tell the user when the walk is over and the list is still
    /// empty. Total over the state, so no ending can be forgotten.
    pub(crate) fn nothing_found_msg(self) -> Option<&'static str> {
        match self {
            Self::Running => None,
            Self::Listed => Some(NOTHING_TO_PICK_MSG),
            Self::Unreadable => Some(UNREADABLE_DIR_MSG),
        }
    }
}

pub(crate) fn apply_remote_entries(
    resources: &mut HashMap<String, ResourceEntry>,
    entries: Vec<ResourceEntry>,
    removed: Vec<WorkbenchPath>,
) -> (Vec<String>, bool) {
    let mut reset = false;
    for path in removed {
        let path = path.display();
        reset |= resources.remove(&path).is_some();
        reset |= resources
            .remove(&format!("{path}{REMOTE_SEPARATOR}"))
            .is_some();
    }
    let mut added = Vec::new();
    for entry in entries {
        let path = entry.path.display();
        let directory = format!("{path}{REMOTE_SEPARATOR}");
        let (path, previous) = if entry.kind == ResourceKind::Directory {
            (directory, path)
        } else {
            (path, directory)
        };
        reset |= resources.remove(&previous).is_some();
        if resources.insert(path.clone(), entry).is_none() {
            added.push(path);
        }
    }
    (added, reset)
}

/// Walks `root` into `injector`, honouring ignore files and skipping `.git`.
/// The returned receiver reports how the walk ended; `None` means the thread
/// could not be spawned and nothing will arrive.
pub(crate) fn spawn(
    root: PathBuf,
    injector: Injector<()>,
    cancel: Arc<AtomicBool>,
) -> Option<flume::Receiver<Walk>> {
    let (done_tx, done_rx) = flume::bounded(1);
    let spawned = thread::Builder::new()
        .name(THREAD_NAME.into())
        .spawn(move || {
            let overrides = OverrideBuilder::new(&root)
                .add(GIT_OVERRIDE)
                .unwrap()
                .build()
                .unwrap();
            WalkBuilder::new(&root)
                .hidden(false)
                // Depth 0 is the root, which strips to an empty name: a bare
                // separator at the top of every list, selected by default.
                .min_depth(Some(1))
                .overrides(overrides)
                .build_parallel()
                .run(|| {
                    let injector = injector.clone();
                    let cancel = cancel.clone();
                    let root = root.clone();
                    Box::new(move |entry| {
                        if cancel.load(Ordering::Relaxed) {
                            return ignore::WalkState::Quit;
                        }
                        let Ok(entry) = entry else {
                            return ignore::WalkState::Continue;
                        };
                        if !entry
                            .file_type()
                            .is_some_and(|ft| ft.is_file() || ft.is_dir() || ft.is_symlink())
                        {
                            return ignore::WalkState::Continue;
                        }
                        let path = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                        let mut name = path.to_string_lossy().into_owned();
                        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                            name.push(std::path::MAIN_SEPARATOR);
                        }
                        injector.push((), |_, cols| {
                            cols[0] = Utf32String::from(name.as_str());
                        });
                        ignore::WalkState::Continue
                    })
                });
            let _ = done_tx.send(end_state(&root));
        });
    match spawned {
        Ok(_) => Some(done_rx),
        Err(error) => {
            warn!("{WALKER_CRASHED_MSG}: failed to spawn thread: {error}");
            None
        }
    }
}

/// Only the directory itself can say whether an empty walk means "nothing to
/// pick" or "I could not even look". Asking happens here, on the walker thread,
/// where a slow filesystem cannot stall the UI.
fn end_state(root: &Path) -> Walk {
    match std::fs::read_dir(root) {
        Ok(_) => Walk::Listed,
        Err(_) => Walk::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const MISSING_DIR: &str = "gone";

    /// Both endings inject nothing, and the flash is the only trace the user
    /// gets, so this is the difference between "there is nothing here" and "I
    /// could not look".
    #[test]
    fn end_state_tells_an_empty_directory_from_an_unopenable_one() {
        let tmp = TempDir::new().unwrap();

        assert_eq!(end_state(tmp.path()), Walk::Listed);
        assert_eq!(end_state(&tmp.path().join(MISSING_DIR)), Walk::Unreadable);
        assert_eq!(Walk::Listed.nothing_found_msg(), Some(NOTHING_TO_PICK_MSG));
        assert_eq!(
            Walk::Unreadable.nothing_found_msg(),
            Some(UNREADABLE_DIR_MSG)
        );
    }
}
