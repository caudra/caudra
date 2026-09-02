use std::{path::Path, sync::Arc};

use tokio::sync::Mutex;

use crate::{FilesystemError, FilesystemLimits, path_policy::RootPathPolicy};

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
}
