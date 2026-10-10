//! Protocol-neutral workspace identities, resources, and asynchronous service boundaries.

#![forbid(unsafe_code)]

mod attachment;
mod capability;
mod changes;
mod error;
mod identity;
mod path;
pub mod path_components;
mod resource;
mod service;
mod transfer;

pub use attachment::{
    ClientAttachment, ClientAttachmentContent, ClientAttachmentId, LocalDocumentRef, MemoryRef,
    PlanRef,
};
pub use capability::{WorkspaceCapabilities, WorkspaceCapability};
pub use changes::{
    ChangeOperationPreview, ChangeOperationResult, CleanupPreview, CleanupSummary, HolderPage,
    HolderSummary, OpenRecord, PendingRevert, PreparedChangeOperation, RecordLimits, RecordListing,
    RecordPage, RecordRequest, RecordScope, RecordState, RecordSummary, RecordedPath,
    ReleaseSelection, ReleaseSummary, RevertChangeKind, RevertConflict, RevertConflictKind,
    RevertCounts, RevertDirection, RevertPath, RevertPreview, RevertState, RevertStatus,
    UNREVEALED_ROOT, UnrecordedReason, WorkspaceChangeBinder, WorkspaceChangeService,
};
pub use error::{InvalidResponseKind, TransportErrorKind, WorkspaceError};
pub use identity::{
    AuthenticatedPrincipalId, AuthorityIdentity, IdentifierError, ProjectIdentity, ProjectKey,
    SourceTrustAnchor,
};
pub use path::{DirectoryNavigation, WorkspacePath, WorkspacePathError};
pub use resource::{
    CollectionRevision, ContinuationToken, CwdHandle, OperationId, RecordHolder, RecordTicket,
    ResourceId, ResourceKind, ResourceRevision, ResourceScope, ResourceScopeError, RevertId,
    ScmRevision, SessionBindingId, SessionWorkspaceBinding, WatchCursor, WatchSubscriptionId,
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
