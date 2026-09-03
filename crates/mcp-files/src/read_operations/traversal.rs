use std::{fs::FileType, path::PathBuf};

use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{FilesystemError, operations::FilesystemCore, text::check_cancelled};

/// Directory names excluded from broad traversal.
///
/// These hold machine-generated build output and tool caches that are
/// regenerable from source, so scanning them spends the traversal budget
/// without producing results a caller asked for. Dependency *source* trees are
/// deliberately absent: `vendor`, `Pods`, `deps`, and `third_party` contain
/// readable code that callers legitimately search. Generic names such as
/// `build`, `bin`, `out`, and `env` are also absent, because the match is a
/// bare basename at every depth and those names carry real source in many
/// projects.
///
/// Skipping applies only to broad traversal. An explicit path is never skipped,
/// so a caller can still search inside any of these by naming it directly.
pub(super) const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    ".dart_tool",
    ".git",
    ".gradle",
    ".mypy_cache",
    ".next",
    ".nuxt",
    ".parcel-cache",
    ".pytest_cache",
    ".ruff_cache",
    ".stack-work",
    ".svelte-kit",
    ".terraform",
    ".tox",
    ".turbo",
    ".venv",
    "__pycache__",
    "dist",
    "node_modules",
    "target",
    "venv",
];

pub(super) struct ListedFiles {
    pub(super) paths: Vec<PathBuf>,
    pub(super) truncated: bool,
}

pub(super) async fn list_files(
    core: &FilesystemCore,
    root: &std::path::Path,
    token: &CancellationToken,
) -> Result<ListedFiles, FilesystemError> {
    let mut paths = Vec::new();
    let mut discovered = 0usize;
    let mut truncated = false;
    let mut stack = vec![(root.to_path_buf(), None)];
    let allows_protected = core.policy.traversal_allows_protected(root);
    check_cancelled(token)?;
    while let Some((candidate, file_type)) = stack.pop() {
        check_cancelled(token)?;
        let directory = if candidate == root {
            candidate
        } else {
            if !core
                .policy
                .traversal_entry_allowed(allows_protected, &candidate)
            {
                continue;
            }
            let name = candidate.file_name().unwrap_or_default();
            if SKIPPED_DIRECTORY_NAMES
                .iter()
                .any(|skipped| name == *skipped)
            {
                continue;
            }
            // The directory read already carries the entry type on platforms
            // that report it, so the common path costs no extra syscall. Fall
            // back only when the filesystem left it unknown.
            let file_type = match file_type {
                Some(file_type) => file_type,
                None => match fs::symlink_metadata(&candidate).await {
                    Ok(metadata) => metadata.file_type(),
                    Err(_) => {
                        truncated = true;
                        continue;
                    }
                },
            };
            if file_type.is_symlink() {
                continue;
            }
            // Every ancestor was verified canonical and non-symlink, so this
            // path is canonical and needs only the confinement decisions.
            if !core.policy.authorize_canonical_entry(&candidate) {
                continue;
            }
            if file_type.is_file() {
                paths.push(candidate);
                continue;
            }
            if !file_type.is_dir() {
                continue;
            }
            candidate
        };

        let remaining = core.limits.max_traversal_entries.saturating_sub(discovered);
        if remaining == 0 {
            truncated = true;
            continue;
        }
        let scan = read_directory_entries(&directory, remaining, token).await?;
        discovered += scan.entries.len();
        truncated |= scan.truncated;
        // Reverse push preserves lexical depth-first processing while each
        // ReadDir is already closed. Final sorting also stabilizes all callers.
        for entry in scan.entries.into_iter().rev() {
            stack.push(entry);
        }
    }
    paths.sort();
    Ok(ListedFiles { paths, truncated })
}

struct DirectoryScan {
    entries: Vec<(PathBuf, Option<FileType>)>,
    truncated: bool,
}

async fn read_directory_entries(
    path: &std::path::Path,
    maximum_entries: usize,
    token: &CancellationToken,
) -> Result<DirectoryScan, FilesystemError> {
    let mut reader = match fs::read_dir(path).await {
        Ok(reader) => reader,
        Err(_) => {
            return Ok(DirectoryScan {
                entries: Vec::new(),
                truncated: true,
            });
        }
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    loop {
        check_cancelled(token)?;
        match reader.next_entry().await {
            Ok(Some(entry)) if entries.len() < maximum_entries => {
                let file_type = entry.file_type().await.ok();
                entries.push((entry.path(), file_type));
            }
            Ok(Some(_)) => {
                truncated = true;
                break;
            }
            Ok(None) => break,
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(DirectoryScan { entries, truncated })
}
