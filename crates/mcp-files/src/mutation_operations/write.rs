use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    diff::{file_diff, line_index_retained_bytes},
    operations::FilesystemCore,
    text::{FileVersion, check_cancelled, enforce_bytes, read_text_snapshot_required},
    types::{FileWriteInput, FileWriteKind, FileWriteOutput},
};

use super::path_string;

impl FilesystemCore {
    pub(crate) async fn prepare_write_bounded(
        &self,
        file_path: &std::path::Path,
        relative_path: String,
        input: FileWriteInput,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<(String, Option<FileVersion>, FileWriteOutput), FilesystemError> {
        check_cancelled(token)?;
        enforce_bytes("content", &input.content, self.limits.max_write_bytes)?;
        let snapshot =
            match read_text_snapshot_required(file_path, self.limits.max_file_bytes, token).await {
                Ok(snapshot) => Some(snapshot),
                Err(error) if error.is_not_found() => None,
                Err(error) => return Err(error),
            };
        let (existing, version) = snapshot
            .map(|snapshot| (snapshot.content, snapshot.version))
            .unzip();
        let old_content = existing.as_deref().unwrap_or_default();
        enforce_preparation_peak(
            old_content
                .len()
                .saturating_add(input.content.capacity())
                .saturating_add(line_index_retained_bytes(&[old_content, &input.content])),
            maximum_retained_bytes,
        )?;
        let diff = file_diff(
            &self.policy,
            file_path,
            old_content,
            &input.content,
            None,
            self.limits.max_diff_bytes,
        )?;
        let output = FileWriteOutput {
            kind: FileWriteKind::Write,
            path: path_string(file_path),
            relative_path,
            existed: existing.is_some(),
            applied: false,
            diff,
            previous: existing.filter(|content| content.len() <= self.limits.max_previous_bytes),
        };
        Ok((input.content, version, output))
    }
}

fn enforce_preparation_peak(bytes: usize, maximum: usize) -> Result<(), FilesystemError> {
    if bytes > maximum {
        return Err(FilesystemError::message(format!(
            "File write preparation exceeds maximum retained size of {maximum} bytes"
        )));
    }
    Ok(())
}
