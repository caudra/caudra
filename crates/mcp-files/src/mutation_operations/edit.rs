use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    diff::{DiffHunk, file_diff, file_diff_hunks},
    operations::FilesystemCore,
    text::{check_cancelled, enforce_bytes, read_text_snapshot_required},
    types::{FileEditInput, FileEditKind, FileEditOutput},
};

use super::path_string;

/// Beyond this many replacement regions no bounded preview can render them all,
/// so recording more buys nothing and a pathological `replaceAll` falls back to
/// the whole-span diff instead of retaining a span per match.
const MAX_EDIT_SPANS: usize = 1024;

impl FilesystemCore {
    pub(crate) async fn file_edit(
        &self,
        input: FileEditInput,
        token: &CancellationToken,
    ) -> Result<FileEditOutput, FilesystemError> {
        let _guard = self.mutation.lock().await;
        check_cancelled(token)?;
        if input.old_string.is_empty() {
            return Err(FilesystemError::message("oldString cannot be empty"));
        }
        if input.old_string == input.new_string {
            return Err(FilesystemError::message(
                "No changes to apply: strings are identical",
            ));
        }
        let file_path = self.policy.resolve(&input.file_path).await?;
        let snapshot =
            read_text_snapshot_required(&file_path, self.limits.max_file_bytes, token).await?;
        let old_content = &snapshot.content;
        let ending = if old_content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let old_string = line_ending(&input.old_string, ending);
        let new_string = line_ending(&input.new_string, ending);
        let replace_all = input.replace_all.unwrap_or(false);
        let new_content = replace_exact(old_content, &old_string, &new_string, replace_all)?;
        enforce_bytes("edited content", &new_content, self.limits.max_write_bytes)?;
        let diff = match edit_spans(old_content, &old_string, &new_string, replace_all) {
            Some(spans) => file_diff_hunks(
                &self.policy,
                &file_path,
                old_content,
                &new_content,
                &spans,
                None,
                self.limits.max_diff_bytes,
            )?,
            None => file_diff(
                &self.policy,
                &file_path,
                old_content,
                &new_content,
                None,
                self.limits.max_diff_bytes,
            )?,
        };
        self.require_write()?;
        self.commit_write(
            &file_path,
            &new_content,
            token,
            false,
            Some(&snapshot.version),
        )
        .await?;
        Ok(FileEditOutput {
            kind: FileEditKind::Edit,
            path: path_string(&file_path),
            relative_path: self.policy.relative(&file_path)?,
            applied: true,
            diff,
        })
    }
}

fn replace_exact(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, FilesystemError> {
    let Some(first) = content.find(old_string) else {
        return Err(FilesystemError::message(
            "Could not find oldString in the file",
        ));
    };
    if replace_all {
        return Ok(content.replace(old_string, new_string));
    }
    if content.rfind(old_string) != Some(first) {
        return Err(FilesystemError::message(
            "Found multiple matches for oldString; provide more context or set replaceAll",
        ));
    }
    let mut output = String::with_capacity(content.len() - old_string.len() + new_string.len());
    output.push_str(&content[..first]);
    output.push_str(new_string);
    output.push_str(&content[first + old_string.len()..]);
    Ok(output)
}

/// One replacement region, expanded to whole lines. Matches sharing a line
/// merge into one region, so the preview never emits a line twice.
struct OpenSpan {
    from: usize,
    to: usize,
    last_end: usize,
    old_start: usize,
    new_newlines: usize,
}

/// Line spans for each replacement region, so the preview costs the edits
/// rather than the distance between the first and the last. `None` when the
/// regions outrun what any bounded preview could render.
fn edit_spans(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Option<Vec<DiffHunk>> {
    let replacement_newlines = count_newlines(new_string);
    let mut spans: Vec<DiffHunk> = Vec::new();
    let mut open: Option<OpenSpan> = None;
    let mut shift = 0isize;
    // Scanned monotonically with the matches, so mapping every match to a line
    // costs one pass over the content rather than one rescan per match.
    let mut scanned = 0usize;
    let mut line = 0usize;
    let mut line_from = 0usize;
    for (start, matched) in content.match_indices(old_string) {
        for (offset, byte) in content.as_bytes()[scanned..start].iter().enumerate() {
            if *byte == b'\n' {
                line += 1;
                line_from = scanned + offset + 1;
            }
        }
        scanned = start;
        let end = start + matched.len();
        let region_end = match open.as_ref() {
            Some(current) if end <= current.to => current.to,
            _ => content[end..]
                .find('\n')
                .map_or(content.len(), |index| end + index),
        };
        match open.as_mut() {
            Some(current) if line_from <= current.to => {
                current.new_newlines +=
                    count_newlines(&content[current.last_end..start]) + replacement_newlines;
                current.last_end = end;
                current.to = current.to.max(region_end);
            }
            _ => {
                if let Some(previous) = open.take() {
                    spans.push(close_span(content, previous, &mut shift));
                    if spans.len() >= MAX_EDIT_SPANS {
                        return None;
                    }
                }
                open = Some(OpenSpan {
                    from: line_from,
                    to: region_end,
                    last_end: end,
                    old_start: line,
                    new_newlines: count_newlines(&content[line_from..start]) + replacement_newlines,
                });
            }
        }
        if !replace_all {
            break;
        }
    }
    let open = open?;
    spans.push(close_span(content, open, &mut shift));
    Some(spans)
}

fn close_span(content: &str, span: OpenSpan, shift: &mut isize) -> DiffHunk {
    let old_len = 1 + count_newlines(&content[span.from..span.to]);
    let new_len = 1 + span.new_newlines + count_newlines(&content[span.last_end..span.to]);
    let hunk = DiffHunk {
        old_start: span.old_start,
        old_len,
        new_start: span.old_start.saturating_add_signed(*shift),
        new_len,
    };
    *shift += new_len as isize - old_len as isize;
    hunk
}

fn count_newlines(value: &str) -> usize {
    value
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
}

fn line_ending(value: &str, ending: &str) -> String {
    let normalized = value.replace("\r\n", "\n");
    if ending == "\n" {
        normalized
    } else {
        normalized.replace('\n', "\r\n")
    }
}
