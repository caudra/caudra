use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    AuthenticatedPrincipalId, AuthorityIdentity, IdentifierError, ProjectIdentity, WorkspaceError,
    WorkspacePath, identity::validate_identifier,
};

const MAX_RESOURCE_ANCESTORS: usize = 256;

fn validate_opaque_id(value: &str) -> Result<(), IdentifierError> {
    validate_identifier(value)
}

macro_rules! opaque_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                validate_opaque_id(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&"<opaque>")
                    .finish()
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

opaque_id!(
    ResourceId,
    "Opaque authority-issued resource identity; callers must not derive path semantics from it."
);
opaque_id!(
    ResourceRevision,
    "Opaque revision used for conditional reads and mutations."
);
opaque_id!(
    CollectionRevision,
    "Opaque revision identifying the collection observed by a paged operation."
);
opaque_id!(
    CwdHandle,
    "Opaque backend-issued handle identifying the cursor's actual working directory."
);
opaque_id!(
    ContinuationToken,
    "Opaque backend-issued continuation for a bounded collection."
);
opaque_id!(
    OperationId,
    "Opaque backend-issued identity for a durable operation lifecycle."
);
opaque_id!(
    WatchSubscriptionId,
    "Opaque backend-issued watch subscription identity."
);
opaque_id!(
    WatchCursor,
    "Opaque backend-issued watch position accepted by subsequent polls."
);
opaque_id!(
    ScmRevision,
    "Opaque source-control repository, collection, or commit revision."
);
opaque_id!(
    RecordTicket,
    "Backend-issued identity of a change record opened before a call and finished after it."
);
opaque_id!(
    RecordHolder,
    "Caller-issued identity a change record is kept for; Caudra holds records for a root session."
);
opaque_id!(
    RevertId,
    "Opaque durable identity of a revert or unrevert of change records."
);
opaque_id!(
    SessionBindingId,
    "Caller-issued identity that keeps cursors from different sessions or tabs distinct."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    ProjectRoot,
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResourceScopeError {
    #[error("resource scope has too many ancestors")]
    TooDeep,
    #[error("resource scope contains the same resource more than once")]
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ResourceScopeWire")]
/// Root-to-leaf resource identity chain used for scope checks without exposing host paths.
pub struct ResourceScope {
    ancestors: Vec<ResourceId>,
    resource_id: ResourceId,
}

#[derive(Deserialize)]
struct ResourceScopeWire {
    ancestors: Vec<ResourceId>,
    resource_id: ResourceId,
}

impl TryFrom<ResourceScopeWire> for ResourceScope {
    type Error = ResourceScopeError;

    fn try_from(scope: ResourceScopeWire) -> Result<Self, Self::Error> {
        Self::new(scope.ancestors, scope.resource_id)
    }
}

impl ResourceScope {
    pub fn new(
        ancestors: Vec<ResourceId>,
        resource_id: ResourceId,
    ) -> Result<Self, ResourceScopeError> {
        if ancestors.len() > MAX_RESOURCE_ANCESTORS {
            return Err(ResourceScopeError::TooDeep);
        }
        let mut unique = BTreeSet::new();
        if ancestors
            .iter()
            .chain(std::iter::once(&resource_id))
            .any(|id| !unique.insert(id))
        {
            return Err(ResourceScopeError::Duplicate);
        }
        Ok(Self {
            ancestors,
            resource_id,
        })
    }

    pub fn root(resource_id: ResourceId) -> Self {
        Self {
            ancestors: Vec::new(),
            resource_id,
        }
    }

    pub fn ancestors(&self) -> &[ResourceId] {
        &self.ancestors
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    pub fn contains(&self, resource_id: &ResourceId) -> bool {
        &self.resource_id == resource_id || self.ancestors.contains(resource_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Authority resource metadata. Project roots use `None` for `path`.
pub struct WorkspaceResource {
    pub project: ProjectIdentity,
    pub scope: ResourceScope,
    pub path: Option<WorkspacePath>,
    pub kind: ResourceKind,
    pub revision: Option<ResourceRevision>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SessionWorkspaceBindingWire")]
/// Immutable authentication and project context attached to one client session or tab.
pub struct SessionWorkspaceBinding {
    binding_id: SessionBindingId,
    authority: AuthorityIdentity,
    principal: AuthenticatedPrincipalId,
    project: ProjectIdentity,
}

#[derive(Deserialize)]
struct SessionWorkspaceBindingWire {
    binding_id: SessionBindingId,
    authority: AuthorityIdentity,
    principal: AuthenticatedPrincipalId,
    project: ProjectIdentity,
}

impl TryFrom<SessionWorkspaceBindingWire> for SessionWorkspaceBinding {
    type Error = WorkspaceError;

    fn try_from(binding: SessionWorkspaceBindingWire) -> Result<Self, Self::Error> {
        Self::new(
            binding.binding_id,
            binding.authority,
            binding.principal,
            binding.project,
        )
    }
}

impl SessionWorkspaceBinding {
    pub fn new(
        binding_id: SessionBindingId,
        authority: AuthorityIdentity,
        principal: AuthenticatedPrincipalId,
        project: ProjectIdentity,
    ) -> Result<Self, WorkspaceError> {
        if principal.authority() != &authority || project.authority() != &authority {
            return Err(WorkspaceError::IdentityMismatch);
        }
        Ok(Self {
            binding_id,
            authority,
            principal,
            project,
        })
    }

    pub fn binding_id(&self) -> &SessionBindingId {
        &self.binding_id
    }

    pub fn authority(&self) -> &AuthorityIdentity {
        &self.authority
    }

    pub fn principal(&self) -> &AuthenticatedPrincipalId {
        &self.principal
    }

    pub fn project(&self) -> &ProjectIdentity {
        &self.project
    }

    pub fn validate_cursor(
        &self,
        cursor: &WorkspaceCursor,
        generation: u64,
        cwd_handle: &CwdHandle,
    ) -> Result<(), WorkspaceError> {
        if cursor.binding_id != self.binding_id || cursor.project != self.project {
            return Err(WorkspaceError::IdentityMismatch);
        }
        if cursor.generation != generation || &cursor.cwd_handle != cwd_handle {
            return Err(WorkspaceError::StaleCursor);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Mutable navigation position bound to exactly one session binding and project.
pub struct WorkspaceCursor {
    binding_id: SessionBindingId,
    project: ProjectIdentity,
    scope: ResourceScope,
    generation: u64,
    cwd_handle: CwdHandle,
}

impl WorkspaceCursor {
    pub fn new(
        binding: &SessionWorkspaceBinding,
        scope: ResourceScope,
        generation: u64,
        cwd_handle: CwdHandle,
    ) -> Self {
        Self {
            binding_id: binding.binding_id.clone(),
            project: binding.project.clone(),
            scope,
            generation,
            cwd_handle,
        }
    }

    pub fn binding_id(&self) -> &SessionBindingId {
        &self.binding_id
    }

    pub fn project(&self) -> &ProjectIdentity {
        &self.project
    }

    pub fn scope(&self) -> &ResourceScope {
        &self.scope
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn cwd_handle(&self) -> &CwdHandle {
        &self.cwd_handle
    }

    pub fn validate(
        &self,
        binding: &SessionWorkspaceBinding,
        generation: u64,
        cwd_handle: &CwdHandle,
    ) -> Result<(), WorkspaceError> {
        binding.validate_cursor(self, generation, cwd_handle)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, ResourceScopeError, SessionBindingId, SessionWorkspaceBinding,
        SourceTrustAnchor, WorkspaceCursor, WorkspaceError,
    };

    fn authority() -> AuthorityIdentity {
        AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").expect("valid anchor"),
            "test-authority",
            "test-workspace",
            "test-generation",
            "test-namespace",
        )
        .expect("valid authority")
    }

    fn binding(id: &str) -> SessionWorkspaceBinding {
        let authority = authority();
        let principal =
            AuthenticatedPrincipalId::new(authority.clone(), "subject").expect("valid principal");
        let project = ProjectIdentity::new(
            authority.clone(),
            ProjectKey::new("project").expect("valid project key"),
        );
        SessionWorkspaceBinding::new(
            SessionBindingId::new(id).expect("valid binding id"),
            authority,
            principal,
            project,
        )
        .expect("consistent binding")
    }

    #[test]
    fn cursor_is_bound_to_one_session_even_for_the_same_project() {
        let first = binding("tab-a");
        let second = binding("tab-b");
        let cursor = WorkspaceCursor::new(
            &first,
            ResourceScope::root(ResourceId::new("root").expect("valid resource id")),
            3,
            CwdHandle::new("cwd-a").expect("valid cwd handle"),
        );

        assert_eq!(
            first.validate_cursor(
                &cursor,
                3,
                &CwdHandle::new("cwd-a").expect("valid cwd handle")
            ),
            Ok(())
        );
        assert_eq!(
            second.validate_cursor(
                &cursor,
                3,
                &CwdHandle::new("cwd-a").expect("valid cwd handle")
            ),
            Err(WorkspaceError::IdentityMismatch)
        );
    }

    #[test]
    fn cursor_validation_includes_generation_and_backend_handle() {
        let binding = binding("tab-a");
        let cwd_handle = CwdHandle::new("cwd-a").expect("valid cwd handle");
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").expect("valid resource id")),
            3,
            cwd_handle.clone(),
        );

        assert_eq!(cursor.validate(&binding, 3, &cwd_handle), Ok(()));
        assert_eq!(
            cursor.validate(
                &binding,
                4,
                &CwdHandle::new("cwd-b").expect("valid cwd handle")
            ),
            Err(WorkspaceError::StaleCursor)
        );
    }

    #[test]
    fn opaque_handles_are_bounded_and_redacted() {
        const SECRET_HANDLE: &str = "private-backend-handle";
        let handle = CwdHandle::new(SECRET_HANDLE).expect("valid cwd handle");

        assert!(!format!("{handle:?}").contains(SECRET_HANDLE));
        assert!(serde_json::from_str::<CwdHandle>(&format!("\"{}\"", "x".repeat(129))).is_err());
    }

    #[test]
    fn scope_preserves_opaque_ancestry_and_rejects_cycles() {
        let root = ResourceId::new("root").expect("valid resource id");
        let directory = ResourceId::new("directory").expect("valid resource id");
        let file = ResourceId::new("file").expect("valid resource id");
        let scope = ResourceScope::new(vec![root.clone(), directory.clone()], file.clone())
            .expect("valid ancestry");

        assert!(scope.contains(&root));
        assert!(scope.contains(&directory));
        assert!(scope.contains(&file));
        assert_eq!(
            ResourceScope::new(vec![root.clone()], root),
            Err(ResourceScopeError::Duplicate)
        );
    }
}
