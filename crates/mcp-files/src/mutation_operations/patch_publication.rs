use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    operations::FilesystemCore,
    text::{check_cancelled, validate_snapshot},
};

use super::{PlannedChange, PlannedChangeType, path_string};

pub(super) async fn publish_patch(
    core: &FilesystemCore,
    changes: &[PlannedChange],
    token: &CancellationToken,
) -> Result<(), FilesystemError> {
    for change in changes {
        check_cancelled(token)?;
        let source = core.policy.resolve(&path_string(&change.file_path)).await?;
        if source != change.file_path {
            return Err(FilesystemError::message(format!(
                "Path changed during patch: {}",
                change.file_path.to_string_lossy()
            )));
        }
        match change.change_type {
            PlannedChangeType::Delete => {
                validate_source(core, change, token).await?;
                fs::remove_file(&source)
                    .await
                    .map_err(|error| FilesystemError::io_path("Cannot delete", &source, error))?;
            }
            PlannedChangeType::Move => {
                let move_path = change.move_path.as_ref().expect("move has target");
                validate_source(core, change, token).await?;
                let target = core.policy.resolve(&path_string(move_path)).await?;
                core.commit_write(&target, &change.new_content, token, true, None)
                    .await?;
                // Revalidate after target publication so a racing source change
                // is never deleted. A failure may leave a safe duplicate target.
                validate_source(core, change, token).await?;
                fs::remove_file(&source).await.map_err(|error| {
                    FilesystemError::io_path("Cannot delete moved source", &source, error)
                })?;
            }
            PlannedChangeType::Add | PlannedChangeType::Update => {
                core.commit_write(
                    &source,
                    &change.new_content,
                    token,
                    change.change_type == PlannedChangeType::Add,
                    change.expected_source.as_ref(),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn validate_source(
    core: &FilesystemCore,
    change: &PlannedChange,
    token: &CancellationToken,
) -> Result<(), FilesystemError> {
    let expected = change
        .expected_source
        .as_ref()
        .expect("delete and move have an expected source");
    validate_snapshot(
        &change.file_path,
        expected,
        core.limits.max_file_bytes,
        token,
    )
    .await
}
