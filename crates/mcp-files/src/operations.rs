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

    pub(crate) fn root(&self) -> &Path {
        self.policy.root()
    }

    pub(crate) fn should_apply(&self, dry_run: Option<bool>) -> Result<bool, FilesystemError> {
        if dry_run.unwrap_or(false) {
            return Ok(false);
        }
        if !self.allow_write {
            return Err(FilesystemError::message(
                "Filesystem is read-only; restart with write access or use dryRun",
            ));
        }
        Ok(true)
    }
}
