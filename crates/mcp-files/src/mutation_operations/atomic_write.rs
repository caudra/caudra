use std::path::Path;

use tokio::{fs, io::AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    FilesystemError,
    operations::FilesystemCore,
    text::{FileVersion, check_cancelled, validate_snapshot},
};

use super::publication_stale;

impl FilesystemCore {
    pub(crate) async fn commit_write(
        &self,
        requested_path: &str,
        file_path: &Path,
        content: &str,
        token: &CancellationToken,
        exclusive: bool,
        expected: Option<&FileVersion>,
    ) -> Result<(), FilesystemError> {
        check_cancelled(token)?;
        let parent = file_path
            .parent()
            .ok_or_else(|| FilesystemError::message("Cannot determine parent directory"))?;
        fs::create_dir_all(parent)
            .await
            .map_err(|error| FilesystemError::io_path("Cannot create directory", parent, error))?;
        check_cancelled(token)?;
        let existing = fs::metadata(file_path).await.ok();
        let basename = file_path.file_name().unwrap_or_default().to_string_lossy();
        let temporary = parent.join(format!(
            ".{basename}.{}.{}.tmp",
            std::process::id(),
            Uuid::new_v4()
        ));

        let result = async {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await.map_err(|error| {
                FilesystemError::io_path("Cannot create temporary file", &temporary, error)
            })?;
            file.write_all(content.as_bytes()).await.map_err(|error| {
                FilesystemError::io_path("Cannot write temporary file", &temporary, error)
            })?;
            file.flush().await.map_err(|error| {
                FilesystemError::io_path("Cannot flush temporary file", &temporary, error)
            })?;
            set_compatible_permissions(&temporary, existing.as_ref()).await?;
            drop(file);
            check_cancelled(token)?;
            self.validate_prepared_path(requested_path, file_path)
                .await
                .map_err(|_| publication_stale(requested_path))?;
            if let Some(expected) = expected {
                validate_snapshot(file_path, expected, self.limits.max_file_bytes, token).await?;
            } else if fs::symlink_metadata(file_path).await.is_ok() {
                return Err(publication_stale(requested_path));
            }
            if exclusive {
                // A same-filesystem hard link is the portable create-if-absent
                // primitive used by the TypeScript implementation. It closes
                // the race between patch planning and publication.
                fs::hard_link(&temporary, file_path)
                    .await
                    .map_err(|error| {
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            publication_stale(requested_path)
                        } else {
                            FilesystemError::io_path("Cannot publish new file", file_path, error)
                        }
                    })?;
                fs::remove_file(&temporary).await.map_err(|error| {
                    FilesystemError::io_path("Cannot remove temporary file", &temporary, error)
                })?;
            } else {
                fs::rename(&temporary, file_path).await.map_err(|error| {
                    FilesystemError::io_path("Cannot replace file", file_path, error)
                })?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        result
    }
}

#[cfg(unix)]
async fn set_compatible_permissions(
    path: &Path,
    existing: Option<&std::fs::Metadata>,
) -> Result<(), FilesystemError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = existing
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .await
        .map_err(|error| FilesystemError::io_path("Cannot set file permissions", path, error))
}

#[cfg(not(unix))]
async fn set_compatible_permissions(
    _path: &Path,
    _existing: Option<&std::fs::Metadata>,
) -> Result<(), FilesystemError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use crate::{operations::FilesystemCore, text::read_text_snapshot_required};

    #[tokio::test]
    async fn rejects_a_stale_edit_snapshot_before_replacement() {
        let root = tempdir().expect("root");
        let path = root.path().join("value.txt");
        std::fs::write(&path, "original\n").expect("original");
        let core = FilesystemCore::create(root.path(), true, None)
            .await
            .expect("core");
        let token = CancellationToken::new();
        let snapshot = read_text_snapshot_required(&path, core.limits.max_file_bytes, &token)
            .await
            .expect("snapshot");
        std::fs::write(&path, "external change\n").expect("racing write");

        let error = core
            .commit_write(
                "value.txt",
                &path,
                "our change\n",
                &token,
                false,
                Some(&snapshot.version),
            )
            .await
            .expect_err("stale snapshot");
        assert!(error.to_string().contains("changed before publication"));
        assert_eq!(
            std::fs::read_to_string(path).expect("current content"),
            "external change\n"
        );
    }
}
