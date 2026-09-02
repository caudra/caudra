use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    diff::file_diff,
    operations::FilesystemCore,
    text::{check_cancelled, enforce_bytes, exists, read_text_if_exists},
    types::{FileWriteInput, FileWriteKind, FileWriteOutput},
};

use super::path_string;

impl FilesystemCore {
    pub(crate) async fn file_write(
        &self,
        input: FileWriteInput,
        token: &CancellationToken,
    ) -> Result<FileWriteOutput, FilesystemError> {
        let _guard = self.mutation.lock().await;
        check_cancelled(token)?;
        enforce_bytes("content", &input.content, self.limits.max_write_bytes)?;
        let file_path = self.policy.resolve(&input.file_path).await?;
        let old_content = read_text_if_exists(&file_path, self.limits.max_file_bytes, token)
            .await?
            .unwrap_or_default();
        let existed = exists(&file_path).await;
        let diff = file_diff(
            &self.policy,
            &file_path,
            &old_content,
            &input.content,
            None,
            self.limits.max_diff_bytes,
        )?;
        self.require_write()?;
        self.commit_write(&file_path, &input.content, token, false, None)
            .await?;
        Ok(FileWriteOutput {
            kind: FileWriteKind::Write,
            path: path_string(&file_path),
            relative_path: self.policy.relative(&file_path)?,
            existed,
            applied: true,
            diff,
        })
    }
}
