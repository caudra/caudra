//! Protocol-neutral workspace identities, resources, and asynchronous service boundaries.

#![forbid(unsafe_code)]

mod attachment;
mod capability;
mod error;
mod identity;
mod path;
mod resource;
mod service;
mod transfer;

pub use attachment::{
    ClientAttachment, ClientAttachmentContent, ClientAttachmentId, LocalDocumentRef, MemoryRef,
    PlanRef,
};
pub use capability::{WorkspaceCapabilities, WorkspaceCapability};
pub use error::{InvalidResponseKind, TransportErrorKind, WorkspaceError};
pub use identity::{
    AuthenticatedPrincipalId, AuthorityIdentity, IdentifierError, ProjectIdentity, ProjectKey,
    SourceTrustAnchor,
};
pub use path::{DirectoryNavigation, WorkspacePath, WorkspacePathError};
pub use resource::{
    CheckpointId, CollectionRevision, ContinuationToken, CwdHandle, OperationId, ResourceId,
    ResourceKind, ResourceRevision, ResourceScope, ResourceScopeError, RestoreId, ScmRevision,
    SessionBindingId, SessionWorkspaceBinding, SnapshotId, WatchCursor, WatchSubscriptionId,
    WorkspaceCursor, WorkspaceResource,
};
pub use service::*;
pub use transfer::{
    DirectoryPublicationRequest, DirectoryPublicationStatus, DownloadedTransfer,
    LocalPublicationState, LocalTransferAuthorization, LocalTransferCondition,
    LocalTransferDestination, LocalTransferPath, LocalTransferReview, LocalTransferRevision,
    LocalTransferService, LocalTransferSource, PreparedDirectoryPublication,
    PreparedLocalDirectory, PreparedLocalTransfer, PreparedTransferPublication,
    PublishedTransferDirectory, RemoteTransferFile, RemoteTransferStage, SealedTransfer,
    TransferContent, TransferDigest, TransferLimits, TransferMode, TransferPublicationRequest,
    TransferPublicationState, TransferPublicationStatus, WorkspaceTransferService,
};
