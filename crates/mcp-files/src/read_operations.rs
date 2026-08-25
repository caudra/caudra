mod glob;
mod grep;
mod listing_metadata;
mod traversal;

use std::path::Path;

use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    operations::FilesystemCore,
    text::{check_cancelled, decode_text, read_bounded, split_text_lines, truncate_line},
    types::{DirectoryEntryDetail, FileEntryKind, FileReadInput, FileReadOutput},
};

use self::listing_metadata::file_listing_metadata;

impl FilesystemCore {
    pub(crate) async fn file_read(
        &self,
        input: FileReadInput,
        token: &CancellationToken,
    ) -> Result<FileReadOutput, FilesystemError> {
        check_cancelled(token)?;
        let requested = if input.file_path.is_empty() {
            "."
        } else {
            &input.file_path
        };
        let file_path = self.policy.resolve(requested).await?;
        let metadata = match fs::metadata(&file_path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(FilesystemError::message(format!(
                    "File not found: {}",
                    input.file_path
                )));
            }
            Err(error) => {
                return Err(FilesystemError::io_path(
                    "Cannot inspect",
                    &file_path,
                    error,
                ));
            }
        };
        if metadata.is_dir() {
            return self.read_directory(&file_path, token).await;
        }
        if !metadata.is_file() {
            return Err(FilesystemError::message(format!(
                "Path is not a regular file: {}",
                input.file_path
            )));
        }

        let offset = input.offset.unwrap_or(1);
        let limit = input.limit.unwrap_or(self.limits.max_read_lines);
        if offset < 1 {
            return Err(FilesystemError::message(
                "offset must be an integer of 1 or greater",
            ));
        }
        if limit > self.limits.max_read_lines {
            return Err(FilesystemError::message(format!(
                "limit must be an integer between 0 and {}",
                self.limits.max_read_lines
            )));
        }
        let bytes = read_bounded(&file_path, self.limits.max_file_bytes, token).await?;
        crate::text::reject_binary(&file_path, &bytes)?;
        let lines = split_text_lines(&decode_text(&bytes));
        let mut selected = Vec::new();
        let mut numbered = Vec::new();
        let mut output_bytes = 0usize;
        let mut truncated = false;
        for (index, source) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            check_cancelled(token)?;
            let line = truncate_line(source, self.limits.max_line_length);
            let numbered_line = format!("{}: {line}", index + 1);
            let size = numbered_line.len() + usize::from(!numbered.is_empty());
            if output_bytes + size > self.limits.max_read_bytes {
                truncated = true;
                break;
            }
            selected.push(line);
            numbered.push(numbered_line);
            output_bytes += size;
        }
        if offset - 1 + selected.len() < lines.len() {
            truncated = true;
        }
        Ok(FileReadOutput::File {
            path: path_string(&file_path),
            relative_path: self.policy.relative(&file_path)?,
            text: selected.join("\n"),
            numbered_text: numbered.join("\n"),
            line_start: offset,
            line_end: offset + selected.len() - 1,
            total_lines: lines.len(),
            truncated,
        })
    }

    async fn read_directory(
        &self,
        path: &Path,
        token: &CancellationToken,
    ) -> Result<FileReadOutput, FilesystemError> {
        let mut reader = fs::read_dir(path)
            .await
            .map_err(|error| FilesystemError::io_path("Cannot read directory", path, error))?;
        let mut raw_entries = Vec::new();
        let mut scan_truncated = false;
        loop {
            check_cancelled(token)?;
            let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|error| FilesystemError::io_path("Cannot read directory", path, error))?
            else {
                break;
            };
            if raw_entries.len() == self.limits.max_traversal_entries {
                scan_truncated = true;
                break;
            }
            raw_entries.push(entry);
        }
        raw_entries.sort_by_key(|entry| entry.file_name());
        let mut entries = Vec::new();
        let mut entry_details = Vec::new();
        let mut truncated = scan_truncated;
        for entry in raw_entries {
            check_cancelled(token)?;
            if entries.len() == self.limits.max_search_results {
                truncated = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(authorized) = self.policy.resolve(&path_string(&entry.path())).await else {
                continue;
            };
            // A broken symlink remains visible as a file-like directory entry
            // with no metadata, matching Node's Dirent + failed stat behavior.
            let metadata = fs::metadata(&authorized).await.ok();
            let directory = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir);
            let relative_path = format!("{name}{}", if directory { "/" } else { "" });
            let listing = if directory {
                Default::default()
            } else {
                file_listing_metadata(self, &authorized, token).await?
            };
            entries.push(relative_path.clone());
            entry_details.push(DirectoryEntryDetail {
                relative_path,
                kind: if directory {
                    FileEntryKind::Directory
                } else {
                    FileEntryKind::File
                },
                size_bytes: listing.size_bytes,
                line_count: listing.line_count,
            });
        }
        Ok(FileReadOutput::Directory {
            path: path_string(path),
            relative_path: self.policy.relative(path)?,
            entries,
            entry_details,
            truncated,
        })
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
