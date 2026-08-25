use std::path::PathBuf;

use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{FilesystemError, operations::FilesystemCore, text::check_cancelled};

use super::path_string;

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
    let mut stack = vec![root.to_path_buf()];
    check_cancelled(token)?;
    while let Some(candidate) = stack.pop() {
        check_cancelled(token)?;
        let directory = if candidate == root {
            candidate
        } else {
            let name = candidate.file_name().unwrap_or_default();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            let metadata = match fs::symlink_metadata(&candidate).await {
                Ok(metadata) => metadata,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            let authorized = match core.policy.resolve(&path_string(&candidate)).await {
                Ok(authorized) => authorized,
                Err(FilesystemError::RootEscape(_) | FilesystemError::ProtectedPath(_)) => {
                    continue;
                }
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            if metadata.is_file() {
                paths.push(authorized);
                continue;
            }
            if !metadata.is_dir() {
                continue;
            }
            authorized
        };

        let remaining = core.limits.max_traversal_entries.saturating_sub(discovered);
        if remaining == 0 {
            truncated = true;
            continue;
        }
        let scan = read_directory_paths(&directory, remaining, token).await?;
        discovered += scan.paths.len();
        truncated |= scan.truncated;
        // Reverse push preserves lexical depth-first processing while each
        // ReadDir is already closed. Final sorting also stabilizes all callers.
        for path in scan.paths.into_iter().rev() {
            stack.push(path);
        }
    }
    paths.sort();
    Ok(ListedFiles { paths, truncated })
}

struct DirectoryScan {
    paths: Vec<PathBuf>,
    truncated: bool,
}

async fn read_directory_paths(
    path: &std::path::Path,
    maximum_entries: usize,
    token: &CancellationToken,
) -> Result<DirectoryScan, FilesystemError> {
    let mut reader = match fs::read_dir(path).await {
        Ok(reader) => reader,
        Err(_) => {
            return Ok(DirectoryScan {
                paths: Vec::new(),
                truncated: true,
            });
        }
    };
    let mut paths = Vec::new();
    let mut truncated = false;
    loop {
        check_cancelled(token)?;
        match reader.next_entry().await {
            Ok(Some(entry)) if paths.len() < maximum_entries => paths.push(entry.path()),
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
    paths.sort();
    Ok(DirectoryScan { paths, truncated })
}
