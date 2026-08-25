use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    diff::file_diff,
    operations::FilesystemCore,
    text::{check_cancelled, enforce_bytes, read_text_snapshot_required},
    types::{FileEditInput, FileEditKind, FileEditOutput},
};

use super::path_string;

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
        let new_content = replace_exact(
            old_content,
            &old_string,
            &new_string,
            input.replace_all.unwrap_or(false),
        )?;
        enforce_bytes("edited content", &new_content, self.limits.max_write_bytes)?;
        let diff = file_diff(
            &self.policy,
            &file_path,
            old_content,
            &new_content,
            None,
            self.limits.max_diff_bytes,
        )?;
        let applied = self.should_apply(input.dry_run)?;
        if applied {
            self.commit_write(
                &file_path,
                &new_content,
                token,
                false,
                Some(&snapshot.version),
            )
            .await?;
        }
        Ok(FileEditOutput {
            kind: FileEditKind::Edit,
            path: path_string(&file_path),
            relative_path: self.policy.relative(&file_path)?,
            applied,
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

fn line_ending(value: &str, ending: &str) -> String {
    let normalized = value.replace("\r\n", "\n");
    if ending == "\n" {
        normalized
    } else {
        normalized.replace('\n', "\r\n")
    }
}
