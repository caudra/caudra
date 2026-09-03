//! Model-facing renderings for filesystem tool results.
//!
//! A tool result carries two forms. The structured record is the canonical data
//! a program consumes; the content block is what a model reads. Serializing the
//! record into the content block makes every result carry its payload twice, so
//! each output type instead renders the one view a reader needs.
//!
//! A field is dropped from the structured record only when it is exactly
//! derivable from a field that remains, so nothing here is lossy: `numberedText`
//! follows from `text` and `lineStart`, a directory listing from its entry
//! details, and a combined patch from the per-file patches.

use std::borrow::Cow;

use crate::types::{
    FileApplyPatchOutput, FileEditOutput, FileGlobOutput, FileGrepOutput, FileReadOutput,
    FileWriteOutput,
};

/// Renders the content block for a tool result.
pub(crate) trait ModelText {
    fn model_text(&self) -> Cow<'_, str>;
}

impl ModelText for FileReadOutput {
    fn model_text(&self) -> Cow<'_, str> {
        match self {
            Self::File { numbered_text, .. } => Cow::Borrowed(numbered_text),
            Self::Directory { entries, .. } => Cow::Owned(entries.join("\n")),
        }
    }
}

impl ModelText for FileGlobOutput {
    fn model_text(&self) -> Cow<'_, str> {
        let mut text = self
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Without this the content block is indistinguishable from a complete
        // result, so a reader cannot tell that narrowing the query is needed.
        if self.truncated {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&if self.scan_complete {
                format!(
                    "[truncated: showing {} of {} matching files]",
                    self.count, self.total
                )
            } else if self.total > self.count {
                format!(
                    "[truncated: showing {} of at least {} matching files; scan stopped early]",
                    self.count, self.total
                )
            } else {
                // Nothing was withheld by the result cap, so quoting a total
                // that equals the shown count would only look like a complete
                // answer. The scan itself is what was cut short.
                format!(
                    "[truncated: showing {} matching files; scan stopped early]",
                    self.count
                )
            });
        }
        Cow::Owned(text)
    }
}

impl ModelText for FileGrepOutput {
    fn model_text(&self) -> Cow<'_, str> {
        let mut text = self
            .rows
            .iter()
            .map(|row| format!("{}:{}: {}", row.relative_path, row.line, row.text))
            .collect::<Vec<_>>()
            .join("\n");
        if self.truncated {
            if !text.is_empty() {
                text.push('\n');
            }
            // "showing" keeps this from reading as the complete match count:
            // the search stops at its result cap, and the protocol ceiling can
            // shorten the returned rows further.
            text.push_str(&format!(
                "[truncated: showing {} matches from {} of {} files searched]",
                self.matches, self.files_scanned, self.files_listed
            ));
        }
        Cow::Owned(text)
    }
}

impl ModelText for FileWriteOutput {
    fn model_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.diff.patch)
    }
}

impl ModelText for FileEditOutput {
    fn model_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.diff.patch)
    }
}

impl ModelText for FileApplyPatchOutput {
    fn model_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.diff)
    }
}

#[cfg(test)]
mod tests {
    use super::ModelText;
    use crate::types::{FileGlobOutput, FileGrepOutput, FileListing};

    fn glob(count: usize, total: usize, scan_complete: bool, truncated: bool) -> FileGlobOutput {
        FileGlobOutput {
            cwd: "/root".into(),
            relative_path: ".".into(),
            pattern: "**/*.rs".into(),
            files: (0..count)
                .map(|index| FileListing {
                    path: format!("/root/{index}.rs"),
                    relative_path: format!("{index}.rs"),
                    size_bytes: None,
                    line_count: None,
                })
                .collect(),
            count,
            total,
            scan_complete,
            truncated,
        }
    }

    #[test]
    fn a_complete_result_carries_no_marker() {
        assert_eq!(glob(2, 2, true, false).model_text(), "0.rs\n1.rs");
    }

    #[test]
    fn a_truncated_result_states_what_was_withheld() {
        assert_eq!(
            glob(2, 9, true, true).model_text(),
            "0.rs\n1.rs\n[truncated: showing 2 of 9 matching files]"
        );
        assert_eq!(
            glob(2, 9, false, true).model_text(),
            "0.rs\n1.rs\n[truncated: showing 2 of at least 9 matching files; scan stopped early]"
        );
        // A stalled scan that withheld nothing must not quote a total, which
        // would read as a complete answer.
        assert_eq!(
            glob(0, 0, false, true).model_text(),
            "[truncated: showing 0 matching files; scan stopped early]"
        );
    }

    #[test]
    fn a_truncated_grep_result_states_its_scan_coverage() {
        let output = FileGrepOutput {
            cwd: "/root".into(),
            relative_path: ".".into(),
            pattern: "x".into(),
            include: None,
            rows: Vec::new(),
            matches: 0,
            files_scanned: 3,
            files_listed: 40,
            truncated: true,
        };
        assert_eq!(
            output.model_text(),
            "[truncated: showing 0 matches from 3 of 40 files searched]"
        );
    }
}
