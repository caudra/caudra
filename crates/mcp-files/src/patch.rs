use crate::FilesystemError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchHunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

impl PatchHunk {
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateChunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    context: Option<String>,
    end_of_file: bool,
}

pub(crate) fn parse_patch(patch_text: &str) -> Result<Vec<PatchHunk>, FilesystemError> {
    let trimmed = patch_text.trim();
    let unwrapped = strip_heredoc(trimmed).unwrap_or(trimmed);
    let normalized = unwrapped.replace("\r\n", "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let begin = lines
        .iter()
        .position(|line| line.trim() == "*** Begin Patch");
    let end = lines.iter().position(|line| line.trim() == "*** End Patch");
    let (Some(begin), Some(end)) = (begin, end) else {
        return Err(FilesystemError::message(
            "Invalid patch format: missing Begin/End markers",
        ));
    };
    if begin >= end {
        return Err(FilesystemError::message(
            "Invalid patch format: missing Begin/End markers",
        ));
    }

    let mut hunks = Vec::new();
    let mut index = begin + 1;
    while index < end {
        let line = lines[index];
        if line.starts_with("*** Add File:") {
            let path = required_header_path(line, "*** Add File:")?;
            let mut content = Vec::new();
            index += 1;
            while index < end && !lines[index].starts_with("***") {
                let content_line = lines[index];
                let Some(content_line) = content_line.strip_prefix('+') else {
                    return Err(FilesystemError::message("Add File lines must start with +"));
                };
                content.push(content_line);
                index += 1;
            }
            hunks.push(PatchHunk::Add {
                path,
                contents: if content.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", content.join("\n"))
                },
            });
            continue;
        }
        if line.starts_with("*** Delete File:") {
            hunks.push(PatchHunk::Delete {
                path: required_header_path(line, "*** Delete File:")?,
            });
            index += 1;
            continue;
        }
        if line.starts_with("*** Update File:") {
            let path = required_header_path(line, "*** Update File:")?;
            index += 1;
            let mut move_path = None;
            if index < end && lines[index].starts_with("*** Move to:") {
                move_path = Some(required_header_path(lines[index], "*** Move to:")?);
                index += 1;
            }
            let mut chunks = Vec::new();
            while index < end && !lines[index].starts_with("***") {
                let marker = lines[index];
                let Some(marker_body) = marker.strip_prefix("@@") else {
                    return Err(FilesystemError::message(format!(
                        "Expected patch chunk, received: {marker}"
                    )));
                };
                let context = match marker_body.trim() {
                    "" => None,
                    value => Some(value.to_owned()),
                };
                index += 1;
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut end_of_file = false;
                while index < end
                    && !lines[index].starts_with("@@")
                    && (!lines[index].starts_with("***") || lines[index] == "*** End of File")
                {
                    let change = lines[index];
                    if change == "*** End of File" {
                        end_of_file = true;
                        index += 1;
                        break;
                    }
                    if let Some(value) = change.strip_prefix(' ') {
                        old_lines.push(value.to_owned());
                        new_lines.push(value.to_owned());
                    } else if let Some(value) = change.strip_prefix('-') {
                        old_lines.push(value.to_owned());
                    } else if let Some(value) = change.strip_prefix('+') {
                        new_lines.push(value.to_owned());
                    } else {
                        return Err(FilesystemError::message(format!(
                            "Invalid patch line: {change}"
                        )));
                    }
                    index += 1;
                }
                chunks.push(UpdateChunk {
                    old_lines,
                    new_lines,
                    context,
                    end_of_file,
                });
            }
            if chunks.is_empty() {
                return Err(FilesystemError::message(format!(
                    "Update File has no chunks: {path}"
                )));
            }
            hunks.push(PatchHunk::Update {
                path,
                move_path,
                chunks,
            });
            continue;
        }
        if !line.trim().is_empty() {
            return Err(FilesystemError::message(format!(
                "Invalid patch section: {line}"
            )));
        }
        index += 1;
    }
    if hunks.is_empty() {
        return Err(FilesystemError::message(
            "Patch must contain at least one file section",
        ));
    }
    Ok(hunks)
}

pub(crate) fn apply_update_chunks(
    file_path: &str,
    chunks: &[UpdateChunk],
    original: &str,
) -> Result<String, FilesystemError> {
    let body = original.strip_suffix('\n').unwrap_or(original);
    let lines = body.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>();
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut cursor = 0usize;
    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let Some(context_index) = seek(&lines, std::slice::from_ref(context), cursor, false)
            else {
                return Err(FilesystemError::message(format!(
                    "Failed to find context '{context}' in {file_path}"
                )));
            };
            cursor = context_index + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let Some(found) = seek(&lines, &chunk.old_lines, cursor, chunk.end_of_file) else {
            return Err(FilesystemError::message(format!(
                "Failed to find expected lines in {file_path}:\n{}",
                chunk.old_lines.join("\n")
            )));
        };
        replacements.push((found, chunk.old_lines.len(), chunk.new_lines.clone()));
        cursor = found + chunk.old_lines.len();
    }

    // Applying from the end keeps earlier indices valid. Stable sorting also
    // reproduces JavaScript's ordering for multiple insertions at one index.
    replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0));
    let mut next = lines;
    for (start, length, replacement) in replacements {
        next.splice(start..start + length, replacement);
    }
    Ok(format!("{}\n", next.join("\n")))
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.len() > lines.len() {
        return None;
    }
    for comparison in [Comparison::Exact, Comparison::TrimEnd, Comparison::Trim] {
        let first = if eof {
            lines.len().saturating_sub(pattern.len())
        } else {
            start
        };
        let last = if eof {
            first
        } else {
            lines.len().saturating_sub(pattern.len())
        };
        if first > last {
            continue;
        }
        for index in first..=last {
            if index >= start
                && pattern.iter().enumerate().all(|(offset, expected)| {
                    comparison.matches(
                        lines.get(index + offset).map(String::as_str).unwrap_or(""),
                        expected,
                    )
                })
            {
                return Some(index);
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Comparison {
    Exact,
    TrimEnd,
    Trim,
}

impl Comparison {
    fn matches(self, left: &str, right: &str) -> bool {
        match self {
            Self::Exact => left == right,
            Self::TrimEnd => left.trim_end() == right.trim_end(),
            Self::Trim => left.trim() == right.trim(),
        }
    }
}

fn required_header_path(line: &str, prefix: &str) -> Result<String, FilesystemError> {
    let value = line[prefix.len()..].trim();
    if value.is_empty() {
        return Err(FilesystemError::message(format!(
            "{prefix} requires a path"
        )));
    }
    Ok(value.to_owned())
}

fn strip_heredoc(input: &str) -> Option<&str> {
    let newline = input.find('\n')?;
    let mut header = input[..newline].trim();
    if let Some(rest) = header.strip_prefix("cat") {
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        header = rest.trim_start();
    }
    let raw_delimiter = header.strip_prefix("<<")?.trim();
    let delimiter = raw_delimiter
        .strip_prefix(['\'', '"'])
        .and_then(|value| value.strip_suffix(['\'', '"']))
        .unwrap_or(raw_delimiter);
    if delimiter.is_empty()
        || !delimiter
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return None;
    }
    let body_and_footer = &input[newline + 1..];
    let footer_start = body_and_footer.rfind('\n')?;
    if body_and_footer[footer_start + 1..].trim() != delimiter {
        return None;
    }
    Some(&body_and_footer[..footer_start])
}

#[cfg(test)]
mod tests {
    use super::{PatchHunk, apply_update_chunks, parse_patch};

    #[test]
    fn parses_heredoc_and_applies_context_with_whitespace_fallback() {
        let hunks = parse_patch(
            "cat <<'PATCH'\n*** Begin Patch\n*** Update File: a.txt\n@@ title\n-old   \n+new\n*** End Patch\nPATCH",
        )
        .expect("patch parses");
        let PatchHunk::Update { chunks, .. } = &hunks[0] else {
            panic!("expected update");
        };
        assert_eq!(
            apply_update_chunks("a.txt", chunks, "title\nold\n").expect("chunk applies"),
            "title\nnew\n"
        );
    }
}
