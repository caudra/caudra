use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    glob::{GlobMatcher, MatchOutcome, MatchScratch},
    operations::FilesystemCore,
    text::check_cancelled,
    types::{FileGlobInput, FileGlobOutput, FileListing},
};

use super::{
    listing_metadata::file_listing_metadata, path_string, relative_to, traversal::list_files,
};

impl FilesystemCore {
    pub(crate) async fn file_glob(
        &self,
        input: FileGlobInput,
        token: &CancellationToken,
    ) -> Result<FileGlobOutput, FilesystemError> {
        if input.pattern.is_empty() {
            return Err(FilesystemError::message("pattern is required"));
        }
        let requested = input
            .path
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let search = self.policy.resolve(requested).await?;
        let metadata = fs::metadata(&search).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FilesystemError::message(format!("Path not found: {requested}"))
            } else {
                FilesystemError::io_path("Cannot inspect", &search, error)
            }
        })?;
        if !metadata.is_dir() {
            return Err(FilesystemError::message(format!(
                "glob path must be a directory: {requested}"
            )));
        }
        let matcher = GlobMatcher::new(&input.pattern, &self.limits)?;
        let listed = list_files(self, &search, token).await?;
        let mut files = Vec::new();
        let mut truncated = listed.truncated;
        let mut match_steps = self.limits.max_glob_match_steps;
        let mut scratch = MatchScratch::default();
        let mut total = 0usize;
        let mut scan_complete = !listed.truncated;
        for file in listed.paths {
            check_cancelled(token)?;
            let relative_path = relative_to(&search, &file);
            let basename = file.file_name().unwrap_or_default().to_string_lossy();
            match matcher.matches_candidate(
                &relative_path,
                &basename,
                &mut match_steps,
                &mut scratch,
            )? {
                MatchOutcome::Matched => {}
                MatchOutcome::Missed => continue,
                // Exhausting the work budget truncates the result. Returning an
                // error here would discard every match already collected.
                MatchOutcome::BudgetExhausted => {
                    truncated = true;
                    scan_complete = false;
                    break;
                }
            }
            total += 1;
            // Counting continues past the returned window so the caller learns
            // how much was withheld. Only the listing metadata is skipped,
            // because it reads the file to count lines.
            if files.len() == self.limits.max_search_results {
                truncated = true;
                continue;
            }
            let metadata = file_listing_metadata(self, &file, token).await?;
            files.push(FileListing {
                path: path_string(&file),
                relative_path,
                size_bytes: metadata.size_bytes,
                line_count: metadata.line_count,
            });
        }
        Ok(FileGlobOutput {
            cwd: path_string(&search),
            relative_path: self.policy.relative(&search)?,
            pattern: input.pattern,
            count: files.len(),
            total,
            scan_complete,
            files,
            truncated,
        })
    }
}
