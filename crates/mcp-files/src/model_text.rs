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
        Cow::Owned(
            self.files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

impl ModelText for FileGrepOutput {
    fn model_text(&self) -> Cow<'_, str> {
        Cow::Owned(
            self.rows
                .iter()
                .map(|row| format!("{}:{}: {}", row.relative_path, row.line, row.text))
                .collect::<Vec<_>>()
                .join("\n"),
        )
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
