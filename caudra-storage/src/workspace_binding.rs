use std::fmt;

use crate::{id::CaudraId, sessions::SessionError};
use caudra_workspace::{
    AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
    SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor, WorkspaceCursor,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const BINDING_VERSION: u32 = 2;
const HASH_DOMAIN: &[u8] = b"caudra.workspace-identity.v1\0";
const LOCAL_SOURCE: &str = "caudra:local:v1";
const MAX_CURSOR_LABEL_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceBindingError {
    #[error("workspace cursor label exceeds {MAX_CURSOR_LABEL_BYTES} bytes")]
    LabelTooLong,
    #[error("workspace cursor label contains a control character")]
    InvalidLabel,
    #[error("workspace identity is inconsistent")]
    IdentityMismatch,
    #[error("unsupported workspace binding version {0}")]
    UnsupportedVersion(u32),
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(into = "StoredWorkspaceBindingWire")]
pub struct StoredWorkspaceBinding {
    binding: SessionWorkspaceBinding,
    cwd_handle: CwdHandle,
    cursor_label: Option<String>,
    cursor: Option<WorkspaceCursor>,
    sandbox_record: Option<CaudraId>,
}

#[derive(Serialize, Deserialize)]
struct StoredWorkspaceBindingWire {
    version: u32,
    binding: SessionWorkspaceBinding,
    cwd_handle: CwdHandle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<WorkspaceCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_record: Option<CaudraId>,
}

impl StoredWorkspaceBinding {
    pub fn is_local(&self) -> bool {
        self.trust_anchor().as_str() == LOCAL_SOURCE
    }

    pub fn validate_resume(
        stored: Option<&Self>,
        expected: Option<&Self>,
    ) -> Result<(), SessionError> {
        let stored = stored.filter(|binding| !binding.is_local());
        let expected = expected.filter(|binding| !binding.is_local());
        match (stored, expected) {
            (None, None) => Ok(()),
            (Some(stored), Some(expected)) if stored.exact_scope_eq(expected) => Ok(()),
            _ => Err(SessionError::WorkspaceRebindRequired),
        }
    }

    pub fn validate_resume_identity(
        stored: Option<&Self>,
        expected: Option<&Self>,
    ) -> Result<(), SessionError> {
        match (
            stored.filter(|binding| !binding.is_local()),
            expected.filter(|binding| !binding.is_local()),
        ) {
            (None, None) => Ok(()),
            (Some(stored), Some(expected)) if stored.same_workspace_identity(expected) => Ok(()),
            _ => Err(SessionError::WorkspaceRebindRequired),
        }
    }

    pub fn new(
        binding: SessionWorkspaceBinding,
        cwd_handle: CwdHandle,
        cursor_label: Option<String>,
    ) -> Result<Self, WorkspaceBindingError> {
        validate_label(cursor_label.as_deref())?;
        Ok(Self {
            binding,
            cwd_handle,
            cursor_label,
            cursor: None,
            sandbox_record: None,
        })
    }

    pub fn new_with_cursor(
        binding: SessionWorkspaceBinding,
        cursor: WorkspaceCursor,
        cursor_label: Option<String>,
    ) -> Result<Self, WorkspaceBindingError> {
        if cursor.binding_id() != binding.binding_id() || cursor.project() != binding.project() {
            return Err(WorkspaceBindingError::IdentityMismatch);
        }
        validate_label(cursor_label.as_deref())?;
        Ok(Self {
            cwd_handle: cursor.cwd_handle().clone(),
            binding,
            cursor_label,
            cursor: Some(cursor),
            sandbox_record: None,
        })
    }

    pub fn local_from_cwd(cwd: &str) -> Self {
        let trust_anchor = SourceTrustAnchor::new(LOCAL_SOURCE).expect("valid local source");
        let authority = AuthorityIdentity::new(
            trust_anchor,
            opaque_hash("local-authority", cwd.as_bytes()),
            opaque_hash("local-workspace", cwd.as_bytes()),
            opaque_hash("local-generation", cwd.as_bytes()),
            "local:v1",
        )
        .expect("hashed local authority");
        let principal = AuthenticatedPrincipalId::new(
            authority.clone(),
            opaque_hash("local-principal", cwd.as_bytes()),
        )
        .expect("hashed local principal");
        let project = ProjectIdentity::new(authority.clone(), local_project_key(cwd));
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new(opaque_hash("local-binding", cwd.as_bytes()))
                .expect("hashed local binding"),
            authority,
            principal,
            project,
        )
        .expect("consistent local binding");
        Self {
            binding,
            cwd_handle: CwdHandle::new(opaque_hash("local-cursor", cwd.as_bytes()))
                .expect("hashed local cursor"),
            cursor_label: None,
            cursor: None,
            sandbox_record: None,
        }
    }

    pub fn binding(&self) -> &SessionWorkspaceBinding {
        &self.binding
    }

    pub fn cwd_handle(&self) -> &CwdHandle {
        &self.cwd_handle
    }

    pub fn cursor_label(&self) -> Option<&str> {
        self.cursor_label.as_deref()
    }

    pub fn cursor(&self) -> Option<&WorkspaceCursor> {
        self.cursor.as_ref()
    }

    pub fn with_cursor(&self, cursor: WorkspaceCursor) -> Result<Self, WorkspaceBindingError> {
        let mut binding =
            Self::new_with_cursor(self.binding.clone(), cursor, self.cursor_label.clone())?;
        binding.sandbox_record = self.sandbox_record;
        Ok(binding)
    }

    pub fn sandbox_record(&self) -> Option<CaudraId> {
        self.sandbox_record
    }

    pub fn with_sandbox_record(mut self, record: CaudraId) -> Result<Self, WorkspaceBindingError> {
        if self.is_local() || self.sandbox_record.is_some_and(|current| current != record) {
            return Err(WorkspaceBindingError::IdentityMismatch);
        }
        self.sandbox_record = Some(record);
        Ok(self)
    }

    pub fn trust_anchor(&self) -> &SourceTrustAnchor {
        self.binding.authority().trust_anchor()
    }

    pub fn server_id(&self) -> &str {
        self.binding.authority().server_id()
    }

    pub fn workspace_id(&self) -> &str {
        self.binding.authority().workspace_id()
    }

    pub fn workspace_generation(&self) -> &str {
        self.binding.authority().workspace_generation()
    }

    pub fn resource_namespace_version(&self) -> &str {
        self.binding.authority().resource_namespace_version()
    }

    pub fn authority_storage_key(&self) -> String {
        opaque_hash(
            "durable-authority",
            &serde_json::to_vec(self.binding.authority())
                .expect("authority identity serialization cannot fail"),
        )
    }

    pub fn matches_authority_storage_key(&self, key: &str) -> bool {
        self.authority_storage_key() == key
            || self.binding.authority().legacy_local_authority_id() == Some(key)
    }

    pub fn principal_id(&self) -> &str {
        self.binding.principal().subject()
    }

    pub fn project_key(&self) -> &ProjectKey {
        self.binding.project().key()
    }

    pub fn exact_scope_eq(&self, other: &Self) -> bool {
        self.trust_anchor() == other.trust_anchor()
            && self.binding.authority() == other.binding.authority()
            && self.principal_id() == other.principal_id()
            && self.project_key() == other.project_key()
            && self.cwd_handle() == other.cwd_handle()
            && self.cursor_label() == other.cursor_label()
            && match (self.cursor(), other.cursor()) {
                (Some(left), Some(right)) => {
                    left.project() == right.project()
                        && left.scope() == right.scope()
                        && left.generation() == right.generation()
                        && left.cwd_handle() == right.cwd_handle()
                }
                (None, None) => true,
                _ => false,
            }
    }

    pub fn same_workspace_identity(&self, other: &Self) -> bool {
        self.trust_anchor() == other.trust_anchor()
            && self.binding.authority() == other.binding.authority()
            && self.principal_id() == other.principal_id()
            && self.project_key() == other.project_key()
    }

    /// Names the change store a remote session records in. Equal exactly
    /// when [`Self::same_workspace_identity`] holds, so a remote cd, which
    /// only moves the cursor, keeps the store.
    pub fn change_store_key(&self) -> String {
        opaque_hash(
            "change-store",
            &serde_json::to_vec(&(
                self.binding.authority(),
                self.principal_id(),
                self.project_key(),
            ))
            .expect("workspace identity serialization cannot fail"),
        )
    }
}

impl fmt::Debug for StoredWorkspaceBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredWorkspaceBinding")
            .field("binding", &self.binding)
            .field("cwd_handle", &self.cwd_handle)
            .field(
                "cursor_label",
                &self.cursor_label.as_ref().map(|_| "<redacted>"),
            )
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl From<StoredWorkspaceBinding> for StoredWorkspaceBindingWire {
    fn from(binding: StoredWorkspaceBinding) -> Self {
        Self {
            version: BINDING_VERSION,
            binding: binding.binding,
            cwd_handle: binding.cwd_handle,
            cursor_label: binding.cursor_label,
            cursor: binding.cursor,
            sandbox_record: binding.sandbox_record,
        }
    }
}

impl<'de> Deserialize<'de> for StoredWorkspaceBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = StoredWorkspaceBindingWire::deserialize(deserializer)?;
        if wire.version != BINDING_VERSION
            && !(wire.version == 1
                && wire.binding.authority().trust_anchor().as_str() == LOCAL_SOURCE)
        {
            return Err(serde::de::Error::custom(
                WorkspaceBindingError::UnsupportedVersion(wire.version),
            ));
        }
        let binding = match wire.cursor {
            Some(cursor) if cursor.cwd_handle() != &wire.cwd_handle => Err(
                serde::de::Error::custom(WorkspaceBindingError::IdentityMismatch),
            ),
            Some(cursor) => Self::new_with_cursor(wire.binding, cursor, wire.cursor_label)
                .map_err(serde::de::Error::custom),
            None => Self::new(wire.binding, wire.cwd_handle, wire.cursor_label)
                .map_err(serde::de::Error::custom),
        }?;
        match wire.sandbox_record {
            Some(record) => binding
                .with_sandbox_record(record)
                .map_err(serde::de::Error::custom),
            None => Ok(binding),
        }
    }
}

fn validate_label(label: Option<&str>) -> Result<(), WorkspaceBindingError> {
    let Some(label) = label else {
        return Ok(());
    };
    if label.len() > MAX_CURSOR_LABEL_BYTES {
        return Err(WorkspaceBindingError::LabelTooLong);
    }
    if label.chars().any(char::is_control) {
        return Err(WorkspaceBindingError::InvalidLabel);
    }
    Ok(())
}

/// The project key [`StoredWorkspaceBinding::local_from_cwd`] gives `cwd`.
pub(crate) fn local_project_key(cwd: &str) -> ProjectKey {
    ProjectKey::new(opaque_hash("local-project", cwd.as_bytes())).expect("hashed local project")
}

pub(crate) fn opaque_hash(purpose: &str, value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update((purpose.len() as u64).to_be_bytes());
    hasher.update(purpose.as_bytes());
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
    bs58::encode(hasher.finalize()).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_workspace::{ResourceId, ResourceScope};
    use test_case::test_case;

    const MISSING_CWD: &str = "/definitely/missing/workspace";
    const REMOTE_SOURCE: &str = "https://remote.example";
    const AUTHORITY: &str = "authority";
    const PRINCIPAL: &str = "principal";
    const PROJECT: &str = "project";
    const CURSOR: &str = "remote-cwd";
    const OTHER: &str = "other";

    /// `cursor` names both the binding and its cwd, which a reconnect and a
    /// remote cd move without leaving the workspace.
    fn remote_binding(
        authority: &str,
        principal: &str,
        project: &str,
        cursor: &str,
    ) -> StoredWorkspaceBinding {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new(REMOTE_SOURCE).unwrap(),
            authority,
            authority,
            authority,
            authority,
        )
        .unwrap();
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new(cursor).unwrap(),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), principal).unwrap(),
            ProjectIdentity::new(authority, ProjectKey::new(project).unwrap()),
        )
        .unwrap();
        StoredWorkspaceBinding::new(binding, CwdHandle::new(cursor).unwrap(), None).unwrap()
    }

    #[test_case(AUTHORITY, PRINCIPAL, PROJECT, OTHER, true ; "another_cursor_keeps_the_store")]
    #[test_case(OTHER, PRINCIPAL, PROJECT, CURSOR, false ; "another_authority_has_its_own_store")]
    #[test_case(AUTHORITY, OTHER, PROJECT, CURSOR, false ; "another_principal_has_its_own_store")]
    #[test_case(AUTHORITY, PRINCIPAL, OTHER, CURSOR, false ; "another_project_has_its_own_store")]
    fn the_change_store_is_named_by_the_workspace_identity(
        authority: &str,
        principal: &str,
        project: &str,
        cursor: &str,
        same: bool,
    ) {
        let base = remote_binding(AUTHORITY, PRINCIPAL, PROJECT, CURSOR);
        let other = remote_binding(authority, principal, project, cursor);

        assert_eq!(base.same_workspace_identity(&other), same);
        assert_eq!(base.change_store_key() == other.change_store_key(), same);
    }

    fn resume_binding(kind: &str) -> Option<StoredWorkspaceBinding> {
        if kind == "legacy" {
            return None;
        }
        let local = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);
        if kind == "local" {
            return Some(local);
        }
        let serialized = serde_json::to_string(&local)
            .unwrap()
            .replace(LOCAL_SOURCE, "https://remote.example");
        Some(serde_json::from_str(&serialized).unwrap())
    }

    #[test_case("legacy", "legacy", true)]
    #[test_case("local", "legacy", true)]
    #[test_case("legacy", "local", true)]
    #[test_case("local", "local", true)]
    #[test_case("remote", "legacy", false)]
    #[test_case("remote", "local", false)]
    #[test_case("legacy", "remote", false)]
    #[test_case("local", "remote", false)]
    #[test_case("remote", "remote", true)]
    fn resume_validates_both_sides(stored: &str, expected: &str, allowed: bool) {
        let stored = resume_binding(stored);
        let expected = resume_binding(expected);
        let result = StoredWorkspaceBinding::validate_resume(stored.as_ref(), expected.as_ref());
        assert_eq!(result.is_ok(), allowed);
        if !allowed {
            assert!(matches!(result, Err(SessionError::WorkspaceRebindRequired)));
        }
    }

    fn downgrade_local_authorities(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    downgrade_local_authorities(value);
                }
            }
            serde_json::Value::Object(fields) if fields.contains_key("server_id") => {
                let trust_anchor = fields.remove("trust_anchor").unwrap();
                let authority_id = fields.remove("server_id").unwrap();
                fields.clear();
                fields.insert("trust_anchor".into(), trust_anchor);
                fields.insert("authority_id".into(), authority_id);
            }
            serde_json::Value::Object(fields) => {
                for value in fields.values_mut() {
                    downgrade_local_authorities(value);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn legacy_local_binding_is_deterministic_without_touching_the_path() {
        let first = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);
        let second = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);

        assert_eq!(first, second);
        assert!(!format!("{first:?}").contains(MISSING_CWD));
        assert!(!serde_json::to_string(&first).unwrap().contains(MISSING_CWD));
    }

    #[test]
    fn hash_purposes_separate_opaque_identifiers() {
        let binding = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);

        assert_ne!(
            binding.binding().binding_id().as_str(),
            binding.cwd_handle().as_str()
        );
        assert_ne!(binding.project_key().as_str(), binding.principal_id());
    }

    #[test]
    fn serialized_binding_is_versioned_and_rejects_unknown_versions() {
        let binding = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);
        let mut value = serde_json::to_value(&binding).unwrap();
        assert_eq!(value["version"], BINDING_VERSION);
        value["version"] = serde_json::json!(BINDING_VERSION + 1);

        assert!(serde_json::from_value::<StoredWorkspaceBinding>(value).is_err());
    }

    #[test_case(1; "old_remote_version")]
    #[test_case(3; "future_remote_version")]
    fn unsupported_remote_binding_versions_are_rejected(version: u32) {
        let mut value = serde_json::to_value(resume_binding("remote").unwrap()).unwrap();
        value["version"] = serde_json::json!(version);
        let error = serde_json::from_value::<StoredWorkspaceBinding>(value).unwrap_err();
        assert_eq!(
            error.to_string(),
            WorkspaceBindingError::UnsupportedVersion(version).to_string()
        );
    }

    #[test]
    fn old_local_workspace_bindings_still_load_with_their_existing_storage_key() {
        let binding = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);
        let legacy_key = binding.server_id().to_owned();
        let mut value = serde_json::to_value(binding).unwrap();
        value["version"] = serde_json::json!(1);
        downgrade_local_authorities(&mut value);

        let restored: StoredWorkspaceBinding = serde_json::from_value(value).unwrap();

        assert!(restored.matches_authority_storage_key(&legacy_key));
        let current_key = restored.authority_storage_key();
        let rewritten: StoredWorkspaceBinding =
            serde_json::from_str(&serde_json::to_string(&restored).unwrap()).unwrap();
        assert!(rewritten.matches_authority_storage_key(&current_key));
        assert!(restored.same_workspace_identity(&rewritten));
    }

    #[test]
    fn remote_cursor_round_trips_and_changes_exact_scope() {
        let root = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);
        let cursor = WorkspaceCursor::new(
            root.binding(),
            ResourceScope::root(ResourceId::new("remote-root").unwrap()),
            4,
            CwdHandle::new("remote-cwd").unwrap(),
        );
        let changed = root.with_cursor(cursor.clone()).unwrap();
        let restored: StoredWorkspaceBinding =
            serde_json::from_str(&serde_json::to_string(&changed).unwrap()).unwrap();

        assert_eq!(restored.cursor(), Some(&cursor));
        assert_eq!(restored.cwd_handle(), cursor.cwd_handle());
        assert!(restored.exact_scope_eq(&changed));
        assert!(!restored.exact_scope_eq(&root));
    }

    #[test]
    fn sandbox_provenance_survives_cursor_and_wire_roundtrip_without_changing_authority() {
        let original = resume_binding("remote").unwrap();
        let id = CaudraId::generate();
        let tagged = original.clone().with_sandbox_record(id).unwrap();
        let cursor = WorkspaceCursor::new(
            tagged.binding(),
            ResourceScope::root(ResourceId::new("sandbox-root").unwrap()),
            0,
            tagged.cwd_handle().clone(),
        );
        let tagged = tagged.with_cursor(cursor).unwrap();
        let restored: StoredWorkspaceBinding =
            serde_json::from_slice(&serde_json::to_vec(&tagged).unwrap()).unwrap();
        assert_eq!(restored.sandbox_record(), Some(id));
        assert!(restored.same_workspace_identity(&original));
        assert!(restored.with_sandbox_record(CaudraId::generate()).is_err());
        assert!(
            StoredWorkspaceBinding::local_from_cwd(MISSING_CWD)
                .with_sandbox_record(id)
                .is_err()
        );
    }

    #[test]
    fn reconnecting_binding_preserves_logical_workspace_and_exact_cursor_scope() {
        let seed = StoredWorkspaceBinding::local_from_cwd(MISSING_CWD);
        let first = seed
            .with_cursor(WorkspaceCursor::new(
                seed.binding(),
                ResourceScope::root(ResourceId::new("remote-root").unwrap()),
                4,
                CwdHandle::new("remote-cwd").unwrap(),
            ))
            .unwrap();
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("reconnected-binding").unwrap(),
            first.binding().authority().clone(),
            first.binding().principal().clone(),
            first.binding().project().clone(),
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            first.cursor().unwrap().scope().clone(),
            first.cursor().unwrap().generation(),
            first.cwd_handle().clone(),
        );
        let reconnected = StoredWorkspaceBinding::new_with_cursor(
            binding,
            cursor,
            first.cursor_label().map(str::to_owned),
        )
        .unwrap();

        assert!(first.same_workspace_identity(&reconnected));
        assert!(first.exact_scope_eq(&reconnected));
    }
}
