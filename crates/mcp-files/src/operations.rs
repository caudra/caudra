use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::Mutex;

use crate::{
    FileResource, FileResourceAccess, FilesystemError, FilesystemLimits,
    path_policy::RootPathPolicy,
};

#[derive(Debug)]
pub(crate) struct FilesystemCore {
    pub(crate) policy: RootPathPolicy,
    pub(crate) allow_write: bool,
    pub(crate) limits: FilesystemLimits,
    /// All mutating calls through one clone share this lock. Planning belongs
    /// inside the critical section so a preview and commit cannot observe two
    /// different states due to another call from the same tool group.
    pub(crate) mutation: Arc<Mutex<()>>,
}

impl FilesystemCore {
    pub(crate) async fn create(
        root: &Path,
        allow_write: bool,
        limits: Option<FilesystemLimits>,
    ) -> Result<Self, FilesystemError> {
        let policy = RootPathPolicy::create(root).await?;
        let limits = limits.unwrap_or_default().validate()?;
        Ok(Self {
            policy,
            allow_write,
            limits,
            mutation: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) async fn create_unconfined(
        base_cwd: &Path,
        allow_write: bool,
        limits: Option<FilesystemLimits>,
    ) -> Result<Self, FilesystemError> {
        let policy = RootPathPolicy::create_unconfined(base_cwd).await?;
        let limits = limits.unwrap_or_default().validate()?;
        Ok(Self {
            policy,
            allow_write,
            limits,
            mutation: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        self.policy.root()
    }

    pub(crate) async fn resolve_directory(
        &self,
        requested: &str,
    ) -> Result<(PathBuf, String), FilesystemError> {
        let path = self.policy.resolve(requested).await?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|_| FilesystemError::message("directory is unavailable"))?;
        if !metadata.is_dir() {
            return Err(FilesystemError::message("path is not a directory"));
        }
        let relative_path = self.policy.relative(&path)?;
        Ok((path, relative_path))
    }

    /// Write authority is immutable process configuration, so a call can never
    /// negotiate it. Protocol hosts never reach this because the mutation tools
    /// are absent from a read-only catalog; native hosts calling the group
    /// directly are denied here.
    pub(crate) fn require_write(&self) -> Result<(), FilesystemError> {
        if !self.allow_write {
            return Err(FilesystemError::message(
                "Filesystem is read-only; restart with write access",
            ));
        }
        Ok(())
    }

    pub(crate) async fn validate_prepared_path(
        &self,
        requested_path: &str,
        authorized_path: &Path,
    ) -> Result<(), FilesystemError> {
        let current = self.policy.resolve(requested_path).await.map_err(|_| {
            FilesystemError::message(format!(
                "Prepared resource changed before execution: {requested_path}"
            ))
        })?;
        if current != authorized_path {
            return Err(FilesystemError::message(format!(
                "Prepared resource changed before execution: {requested_path}"
            )));
        }
        Ok(())
    }

    pub(crate) async fn revalidate_resource(
        &self,
        resource: &FileResource,
    ) -> Result<std::fs::Metadata, FilesystemError> {
        let canonical = self.policy.revalidate(&resource.path).await?;
        if canonical != resource.path {
            return Err(FilesystemError::message(format!(
                "Prepared resource changed before execution: {}",
                resource.requested_path
            )));
        }
        let metadata = tokio::fs::metadata(&canonical)
            .await
            .map_err(|error| FilesystemError::io_path("Cannot inspect", &canonical, error))?;
        let expected_type_matches = match resource.access {
            FileResourceAccess::Traverse => metadata.is_dir(),
            FileResourceAccess::Read
            | FileResourceAccess::ReadWrite
            | FileResourceAccess::Delete => metadata.is_file(),
            FileResourceAccess::Write => true,
        };
        if !expected_type_matches {
            return Err(FilesystemError::message(format!(
                "Prepared resource type changed before execution: {}",
                resource.requested_path
            )));
        }
        Ok(metadata)
    }
}
