use regex::Regex;
use std::collections::HashMap;

use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError, PreparedFileGrep,
    glob::{MatchOutcome, MatchScratch},
    operations::FilesystemCore,
    text::{
        check_cancelled, js_length, read_text_snapshot_required, split_text_lines, truncate_line,
    },
    types::{FileGrepOutput, FileGrepRow},
};

use super::{
    path_string, relative_to,
    traversal::{ListedFiles, list_files},
};

impl FilesystemCore {
    pub(crate) async fn file_grep_prepared(
        &self,
        prepared: PreparedFileGrep,
        token: &CancellationToken,
    ) -> Result<FileGrepOutput, FilesystemError> {
        let PreparedFileGrep {
            resources: [resource],
            relative_paths: [relative_root],
            pattern,
            include,
            regex,
            include_matcher,
            ..
        } = prepared;
        let requested = resource.path;
        check_cancelled(token)?;
        let metadata = fs::metadata(&requested).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FilesystemError::message("Prepared grep path is no longer available")
            } else {
                FilesystemError::io_path("Cannot inspect", &requested, error)
            }
        })?;
        let listed = if metadata.is_file() {
            ListedFiles::single(requested.clone())
        } else {
            list_files(self, &requested, token).await?
        };
        let cwd = if metadata.is_file() {
            requested.parent().unwrap_or(&requested).to_path_buf()
        } else {
            requested.clone()
        };
        let mut rows = Vec::new();
        let mut truncated = listed.truncated;
        let mut glob_match_steps = self.limits.max_glob_match_steps;
        let mut scratch = MatchScratch::default();
        let files_listed = listed.paths.len();
        let ignored = listed.ignored;
        let ignore_complete = listed.ignore_complete;
        let mut files_scanned = 0usize;
        let mut revisions = HashMap::new();
        'files: for file in listed.paths {
            check_cancelled(token)?;
            let relative_path = relative_to(&cwd, &file);
            let basename = file.file_name().unwrap_or_default().to_string_lossy();
            if let Some(matcher) = &include_matcher {
                match matcher.matches_candidate(
                    &relative_path,
                    &basename,
                    &mut glob_match_steps,
                    &mut scratch,
                )? {
                    MatchOutcome::Matched => {}
                    MatchOutcome::Missed => continue,
                    // Exhausting the work budget truncates the result rather
                    // than discarding every row already collected.
                    MatchOutcome::BudgetExhausted => {
                        truncated = true;
                        break;
                    }
                }
            }
            files_scanned += 1;
            let snapshot =
                match read_text_snapshot_required(&file, self.limits.max_file_bytes, token).await {
                    Ok(snapshot) => snapshot,
                    Err(FilesystemError::Aborted) => return Err(FilesystemError::Aborted),
                    Err(_) => continue,
                };
            let first_row = rows.len();
            for (index, source) in split_text_lines(&snapshot.content).iter().enumerate() {
                check_cancelled(token)?;
                let line = truncate_line(source, self.limits.max_line_length);
                if regex.find(&line).is_none() {
                    continue;
                }
                if rows.len() == self.limits.max_search_results {
                    truncated = true;
                    break 'files;
                }
                if rows.len() == first_row {
                    revisions.insert(path_string(&file), snapshot.version);
                }
                rows.push(FileGrepRow {
                    path: path_string(&file),
                    relative_path: relative_path.clone(),
                    line: index + 1,
                    text: line,
                });
            }
            if rows.len() == self.limits.max_search_results {
                truncated = true;
                break;
            }
        }
        Ok(FileGrepOutput {
            cwd: path_string(&cwd),
            relative_path: relative_root,
            pattern,
            include,
            matches: rows.len(),
            files_scanned,
            files_listed,
            rows,
            truncated,
            ignored,
            ignore_complete,
            revisions,
        })
    }
}

pub(crate) fn compile_linear_regex(
    pattern: &str,
    maximum: usize,
) -> Result<Regex, FilesystemError> {
    if js_length(pattern) > maximum {
        return Err(FilesystemError::message(format!(
            "grep regex exceeds maximum length of {maximum}"
        )));
    }
    if contains_unsupported_ecmascript_construct(pattern) {
        return Err(FilesystemError::message(
            "Unsupported grep regex construct: look-around and backreferences are not available in linear-time mode",
        ));
    }
    Regex::new(pattern)
        .map_err(|error| FilesystemError::message(format!("Invalid grep regex: {error}")))
}

fn contains_unsupported_ecmascript_construct(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut index = 0usize;
    let mut in_class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                let next = bytes.get(index + 1).copied();
                if !in_class
                    && (next.is_some_and(|byte| matches!(byte, b'1'..=b'9'))
                        || (next == Some(b'k') && bytes.get(index + 2) == Some(&b'<')))
                {
                    return true;
                }
                index += usize::from(next.is_some()) + 1;
            }
            b'[' if !in_class => {
                in_class = true;
                index += 1;
            }
            b']' if in_class => {
                in_class = false;
                index += 1;
            }
            b'(' if !in_class
                && (bytes[index..].starts_with(b"(?=")
                    || bytes[index..].starts_with(b"(?!")
                    || bytes[index..].starts_with(b"(?<=")
                    || bytes[index..].starts_with(b"(?<!")) =>
            {
                return true;
            }
            _ => index += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::compile_linear_regex;

    #[test]
    fn rejects_ecmascript_constructs_that_require_backtracking() {
        for pattern in [r"(a)\1", r"(?=a)a", r"(?<=a)b", r"(?<name>a)\k<name>"] {
            let error = compile_linear_regex(pattern, 1_000).expect_err("unsupported construct");
            assert!(error.to_string().contains("linear-time mode"), "{pattern}");
        }
        assert!(compile_linear_regex(r"[(]a[)]|a+", 1_000).is_ok());
    }
}
