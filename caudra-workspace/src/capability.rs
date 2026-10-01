use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::WorkspaceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Independently negotiable workspace operation families.
pub enum WorkspaceCapability {
    Resolve,
    Stat,
    List,
    ReadText,
    ReadBytes,
    ReviewedTransfer,
    MutationExecute,
    MutationStatus,
    MutationCancel,
    MutationAtomic,
    MutationRollback,
    Search,
    WatchOpen,
    WatchPoll,
    WatchClose,
    WatchRecursive,
    WatchExactRenamePairing,
    ExecExecute,
    ExecStatus,
    ExecCancel,
    ExecTimeout,
    ScmDiscover,
    ScmStatus,
    ScmLog,
    ScmDiff,
    ScmReadSide,
    ScmStage,
    ScmUnstage,
    ScmDiscard,
    ScmMutationStatus,
    ScmMutationCancel,
    ScmMutationRelease,
    /// Per-call change records and the reverts that undo exactly them.
    ChangeRecords,
    ProjectAssetsDiscover,
    ProjectAssetsRead,
    ToolPrepare,
    ToolExecute,
    ToolStatus,
    ToolCancel,
    ToolRelease,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
/// Capabilities advertised by one immutable workspace authority handle.
pub struct WorkspaceCapabilities(BTreeSet<WorkspaceCapability>);

impl WorkspaceCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = WorkspaceCapability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    pub fn supports(&self, capability: WorkspaceCapability) -> bool {
        self.0.contains(&capability)
    }

    pub fn require(&self, capability: WorkspaceCapability) -> Result<(), WorkspaceError> {
        self.supports(capability)
            .then_some(())
            .ok_or(WorkspaceError::UnsupportedCapability { capability })
    }

    pub fn iter(&self) -> impl Iterator<Item = WorkspaceCapability> + '_ {
        self.0.iter().copied()
    }

    pub fn validate(&self) -> Result<(), WorkspaceError> {
        for capability in self.iter() {
            if let Some(required) = capability.required_capability()
                && !self.supports(required)
            {
                return Err(WorkspaceError::CapabilityMismatch { capability });
            }
            if capability.requires_scm_mutation()
                && ![
                    WorkspaceCapability::ScmStage,
                    WorkspaceCapability::ScmUnstage,
                    WorkspaceCapability::ScmDiscard,
                ]
                .into_iter()
                .any(|candidate| self.supports(candidate))
            {
                return Err(WorkspaceError::CapabilityMismatch { capability });
            }
        }
        Ok(())
    }
}

impl WorkspaceCapability {
    const fn required_capability(self) -> Option<Self> {
        match self {
            Self::MutationAtomic | Self::MutationRollback => Some(Self::MutationExecute),
            Self::MutationStatus | Self::MutationCancel => Some(Self::MutationExecute),
            Self::WatchPoll
            | Self::WatchClose
            | Self::WatchRecursive
            | Self::WatchExactRenamePairing => Some(Self::WatchOpen),
            Self::ExecStatus | Self::ExecCancel | Self::ExecTimeout => Some(Self::ExecExecute),
            Self::ProjectAssetsRead => Some(Self::ProjectAssetsDiscover),
            Self::ToolExecute | Self::ToolRelease => Some(Self::ToolPrepare),
            Self::ToolStatus | Self::ToolCancel => Some(Self::ToolExecute),
            _ => None,
        }
    }

    const fn requires_scm_mutation(self) -> bool {
        matches!(
            self,
            Self::ScmMutationStatus | Self::ScmMutationCancel | Self::ScmMutationRelease
        )
    }
}

impl<const N: usize> From<[WorkspaceCapability; N]> for WorkspaceCapabilities {
    fn from(capabilities: [WorkspaceCapability; N]) -> Self {
        Self::new(capabilities)
    }
}

#[cfg(test)]
mod tests {
    use crate::{WorkspaceCapabilities, WorkspaceCapability, WorkspaceError};

    #[test]
    fn capability_checks_are_explicit_and_typed() {
        let capabilities = WorkspaceCapabilities::from([
            WorkspaceCapability::Resolve,
            WorkspaceCapability::ReadText,
        ]);

        assert!(capabilities.supports(WorkspaceCapability::Resolve));
        assert_eq!(
            capabilities.require(WorkspaceCapability::MutationExecute),
            Err(WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::MutationExecute,
            })
        );
    }

    #[test]
    fn field_capabilities_require_the_method_that_can_express_them() {
        let capabilities = WorkspaceCapabilities::from([WorkspaceCapability::ExecTimeout]);

        assert_eq!(
            capabilities.validate(),
            Err(WorkspaceError::CapabilityMismatch {
                capability: WorkspaceCapability::ExecTimeout,
            })
        );
    }
}
