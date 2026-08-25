use regex::Regex;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    glob::GlobMatcher,
    operations::FilesystemCore,
    text::{
        check_cancelled, decode_text, is_binary_content, js_length, read_bounded, split_text_lines,
        truncate_line,
    },
    types::{FileGrepInput, FileGrepOutput, FileGrepRow},
};

use super::{
    path_string, relative_to,
    traversal::{ListedFiles, list_files},
};

impl FilesystemCore {
    pub(crate) async fn file_grep(
        &self,
        input: FileGrepInput,
        token: &CancellationToken,
    ) -> Result<FileGrepOutput, FilesystemError> {
        check_cancelled(token)?;
        if input.pattern.is_empty() {
            return Err(FilesystemError::message("pattern is required"));
        }
        let regex = compile_linear_regex(&input.pattern, self.limits.max_regex_length)?;
        let include = input.include.as_deref().filter(|value| !value.is_empty());
        let include_matcher = include
            .map(|pattern| GlobMatcher::new(pattern, &self.limits))
            .transpose()?;
        let requested_name = input
            .path
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let requested = self.policy.resolve(requested_name).await?;
        let metadata = fs::metadata(&requested).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FilesystemError::message(format!("Path not found: {requested_name}"))
            } else {
                FilesystemError::io_path("Cannot inspect", &requested, error)
            }
        })?;
        let listed = if metadata.is_file() {
            ListedFiles {
                paths: vec![requested.clone()],
                truncated: false,
            }
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
        'files: for file in listed.paths {
            check_cancelled(token)?;
            let relative_path = relative_to(&cwd, &file);
            let basename = file.file_name().unwrap_or_default().to_string_lossy();
            if let Some(matcher) = &include_matcher
                && !matcher.is_match(&relative_path, &mut glob_match_steps)?
                && !matcher.is_match(&basename, &mut glob_match_steps)?
            {
                continue;
            }
            let bytes = match read_bounded(&file, self.limits.max_file_bytes, token).await {
                Ok(bytes) => bytes,
                Err(FilesystemError::Aborted) => return Err(FilesystemError::Aborted),
                Err(_) => continue,
            };
            if is_binary_content(&bytes) {
                continue;
            }
            for (index, source) in split_text_lines(&decode_text(&bytes)).iter().enumerate() {
                check_cancelled(token)?;
                let line = truncate_line(source, self.limits.max_line_length);
                if regex.find(&line).is_none() {
                    continue;
                }
                if rows.len() == self.limits.max_search_results {
                    truncated = true;
                    break 'files;
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
            relative_path: self.policy.relative(&requested)?,
            pattern: input.pattern,
            include: input.include,
            matches: rows.len(),
            rows,
            truncated,
        })
    }
}

fn compile_linear_regex(pattern: &str, maximum: usize) -> Result<Regex, FilesystemError> {
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
