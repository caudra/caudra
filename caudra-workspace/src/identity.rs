use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

pub(crate) const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_TRUST_ANCHOR_BYTES: usize = 2048;
const AUTHORITY_IDENTITY_VERSION: u32 = 2;
const LOCAL_TRUST_ANCHOR: &str = "caudra:local:v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentifierError {
    #[error("identifier must not be empty")]
    Empty,
    #[error("identifier exceeds {MAX_IDENTIFIER_BYTES} bytes")]
    TooLong,
    #[error("source trust anchor exceeds {MAX_TRUST_ANCHOR_BYTES} bytes")]
    TrustAnchorTooLong,
    #[error("identifier contains a control character")]
    ControlCharacter,
}

pub(crate) fn validate_identifier(value: &str) -> Result<(), IdentifierError> {
    if value.is_empty() {
        return Err(IdentifierError::Empty);
    }
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(IdentifierError::TooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(IdentifierError::ControlCharacter);
    }
    Ok(())
}

macro_rules! identifier {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                validate_identifier(&value)?;
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

/// Stable normalized origin or local namespace that established trust in a workspace source.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SourceTrustAnchor(String);

impl SourceTrustAnchor {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentifierError::Empty);
        }
        if value.len() > MAX_TRUST_ANCHOR_BYTES {
            return Err(IdentifierError::TrustAnchorTooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(IdentifierError::ControlCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SourceTrustAnchor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SourceTrustAnchor")
            .field(&"<opaque>")
            .finish()
    }
}

impl TryFrom<String> for SourceTrustAnchor {
    type Error = IdentifierError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SourceTrustAnchor> for String {
    fn from(value: SourceTrustAnchor) -> Self {
        value.0
    }
}
identifier!(
    ProjectKey,
    "Opaque project key whose meaning is scoped to an authority."
);

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
struct OpaqueIdentifier(String);

impl TryFrom<String> for OpaqueIdentifier {
    type Error = IdentifierError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_identifier(&value)?;
        Ok(Self(value))
    }
}

impl From<OpaqueIdentifier> for String {
    fn from(value: OpaqueIdentifier) -> Self {
        value.0
    }
}

#[derive(Clone, Serialize)]
#[serde(into = "AuthorityIdentityWire")]
/// Durable identity of a backend authority, excluding its operational process instance.
pub struct AuthorityIdentity {
    trust_anchor: SourceTrustAnchor,
    server_id: OpaqueIdentifier,
    workspace_id: OpaqueIdentifier,
    workspace_generation: OpaqueIdentifier,
    resource_namespace_version: OpaqueIdentifier,
    legacy_local_authority_id: Option<OpaqueIdentifier>,
}

impl PartialEq for AuthorityIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.trust_anchor == other.trust_anchor
            && self.server_id == other.server_id
            && self.workspace_id == other.workspace_id
            && self.workspace_generation == other.workspace_generation
            && self.resource_namespace_version == other.resource_namespace_version
    }
}

impl Eq for AuthorityIdentity {}

impl Hash for AuthorityIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.trust_anchor.hash(state);
        self.server_id.hash(state);
        self.workspace_id.hash(state);
        self.workspace_generation.hash(state);
        self.resource_namespace_version.hash(state);
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityIdentityWire {
    version: u32,
    trust_anchor: SourceTrustAnchor,
    server_id: OpaqueIdentifier,
    workspace_id: OpaqueIdentifier,
    workspace_generation: OpaqueIdentifier,
    resource_namespace_version: OpaqueIdentifier,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAuthorityIdentityWire {
    trust_anchor: SourceTrustAnchor,
    authority_id: OpaqueIdentifier,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AuthorityIdentityDeserializeWire {
    Current(AuthorityIdentityWire),
    Legacy(LegacyAuthorityIdentityWire),
}

impl AuthorityIdentity {
    pub fn new(
        trust_anchor: SourceTrustAnchor,
        server_id: impl Into<String>,
        workspace_id: impl Into<String>,
        workspace_generation: impl Into<String>,
        resource_namespace_version: impl Into<String>,
    ) -> Result<Self, IdentifierError> {
        Ok(Self {
            trust_anchor,
            server_id: OpaqueIdentifier::try_from(server_id.into())?,
            workspace_id: OpaqueIdentifier::try_from(workspace_id.into())?,
            workspace_generation: OpaqueIdentifier::try_from(workspace_generation.into())?,
            resource_namespace_version: OpaqueIdentifier::try_from(
                resource_namespace_version.into(),
            )?,
            legacy_local_authority_id: None,
        })
    }

    pub fn trust_anchor(&self) -> &SourceTrustAnchor {
        &self.trust_anchor
    }

    pub fn server_id(&self) -> &str {
        &self.server_id.0
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id.0
    }

    pub fn workspace_generation(&self) -> &str {
        &self.workspace_generation.0
    }

    pub fn resource_namespace_version(&self) -> &str {
        &self.resource_namespace_version.0
    }

    pub fn legacy_local_authority_id(&self) -> Option<&str> {
        self.legacy_local_authority_id
            .as_ref()
            .map(|identifier| identifier.0.as_str())
    }
}

impl From<AuthorityIdentity> for AuthorityIdentityWire {
    fn from(identity: AuthorityIdentity) -> Self {
        Self {
            version: AUTHORITY_IDENTITY_VERSION,
            trust_anchor: identity.trust_anchor,
            server_id: identity.server_id,
            workspace_id: identity.workspace_id,
            workspace_generation: identity.workspace_generation,
            resource_namespace_version: identity.resource_namespace_version,
        }
    }
}

impl<'de> Deserialize<'de> for AuthorityIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match AuthorityIdentityDeserializeWire::deserialize(deserializer)? {
            AuthorityIdentityDeserializeWire::Current(wire)
                if wire.version == AUTHORITY_IDENTITY_VERSION =>
            {
                Ok(Self {
                    trust_anchor: wire.trust_anchor,
                    server_id: wire.server_id,
                    workspace_id: wire.workspace_id,
                    workspace_generation: wire.workspace_generation,
                    resource_namespace_version: wire.resource_namespace_version,
                    legacy_local_authority_id: None,
                })
            }
            AuthorityIdentityDeserializeWire::Current(wire) => Err(serde::de::Error::custom(
                format!("unsupported authority identity version {}", wire.version),
            )),
            AuthorityIdentityDeserializeWire::Legacy(wire)
                if wire.trust_anchor.as_str() == LOCAL_TRUST_ANCHOR =>
            {
                Ok(Self {
                    trust_anchor: wire.trust_anchor,
                    server_id: wire.authority_id.clone(),
                    workspace_id: wire.authority_id.clone(),
                    workspace_generation: wire.authority_id.clone(),
                    resource_namespace_version: OpaqueIdentifier::try_from("local:v1".to_owned())
                        .map_err(serde::de::Error::custom)?,
                    legacy_local_authority_id: Some(wire.authority_id),
                })
            }
            AuthorityIdentityDeserializeWire::Legacy(_) => Err(serde::de::Error::custom(
                "legacy remote authority identity is not durable",
            )),
        }
    }
}

impl fmt::Debug for AuthorityIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorityIdentity")
            .field("trust_anchor", &self.trust_anchor)
            .field("server_id", &"<opaque>")
            .field("workspace_id", &"<opaque>")
            .field("workspace_generation", &"<opaque>")
            .field("resource_namespace_version", &"<opaque>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
/// Authenticated subject identity. The subject is meaningful only under its authority.
pub struct AuthenticatedPrincipalId {
    authority: AuthorityIdentity,
    subject: OpaqueIdentifier,
}

impl AuthenticatedPrincipalId {
    pub fn new(
        authority: AuthorityIdentity,
        subject: impl Into<String>,
    ) -> Result<Self, IdentifierError> {
        let subject = subject.into();
        let subject = OpaqueIdentifier::try_from(subject)?;
        Ok(Self { authority, subject })
    }

    pub fn authority(&self) -> &AuthorityIdentity {
        &self.authority
    }

    pub fn subject(&self) -> &str {
        &self.subject.0
    }
}

impl fmt::Debug for AuthenticatedPrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticatedPrincipalId")
            .field("authority", &self.authority)
            .field("subject", &"<opaque>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
/// Globally unambiguous project identity formed from an authority and its opaque project key.
pub struct ProjectIdentity {
    authority: AuthorityIdentity,
    key: ProjectKey,
}

impl ProjectIdentity {
    pub fn new(authority: AuthorityIdentity, key: ProjectKey) -> Self {
        Self { authority, key }
    }

    pub fn authority(&self) -> &AuthorityIdentity {
        &self.authority
    }

    pub fn key(&self) -> &ProjectKey {
        &self.key
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, SourceTrustAnchor,
    };

    const AUTHORITY_ID: &str = "workspace.example";
    const PROJECT_KEY: &str = "project-42";
    const SECRET_ANCHOR: &str = "configured-anchor";

    fn authority(anchor: &str) -> AuthorityIdentity {
        AuthorityIdentity::new(
            SourceTrustAnchor::new(anchor).expect("valid anchor"),
            AUTHORITY_ID,
            "workspace",
            "generation",
            "namespace-v1",
        )
        .expect("valid authority")
    }

    #[test]
    fn authority_namespaces_principals_and_projects() {
        let first = authority("source-a");
        let second = authority("source-b");
        let first_project = ProjectIdentity::new(
            first.clone(),
            ProjectKey::new(PROJECT_KEY).expect("valid project key"),
        );
        let second_project = ProjectIdentity::new(
            second.clone(),
            ProjectKey::new(PROJECT_KEY).expect("valid project key"),
        );
        let first_principal =
            AuthenticatedPrincipalId::new(first, "subject").expect("valid principal");
        let second_principal =
            AuthenticatedPrincipalId::new(second, "subject").expect("valid principal");

        assert_ne!(first_project, second_project);
        assert_ne!(first_principal, second_principal);
    }

    #[test]
    fn every_durable_remote_authority_dimension_changes_identity() {
        let anchor = SourceTrustAnchor::new("https://workcell.example").unwrap();
        let baseline = AuthorityIdentity::new(
            anchor.clone(),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        for other in [
            AuthorityIdentity::new(
                SourceTrustAnchor::new("https://clone.example").unwrap(),
                "server",
                "workspace",
                "generation",
                "namespace",
            )
            .unwrap(),
            AuthorityIdentity::new(
                anchor.clone(),
                "other-server",
                "workspace",
                "generation",
                "namespace",
            )
            .unwrap(),
            AuthorityIdentity::new(
                anchor.clone(),
                "server",
                "other-workspace",
                "generation",
                "namespace",
            )
            .unwrap(),
            AuthorityIdentity::new(
                anchor.clone(),
                "server",
                "workspace",
                "other-generation",
                "namespace",
            )
            .unwrap(),
            AuthorityIdentity::new(
                anchor,
                "server",
                "workspace",
                "generation",
                "other-namespace",
            )
            .unwrap(),
        ] {
            assert_ne!(baseline, other);
        }
    }

    #[test]
    fn opaque_identity_serializes_but_debug_output_is_redacted() {
        let anchor = SourceTrustAnchor::new(SECRET_ANCHOR).expect("valid anchor");
        let json = serde_json::to_string(&anchor).expect("serialize anchor");
        let debug = format!("{anchor:?}");

        assert_eq!(json, format!(r#""{SECRET_ANCHOR}""#));
        assert!(!debug.contains(SECRET_ANCHOR));
        assert!(debug.contains("<opaque>"));
    }

    #[test]
    fn legacy_remote_authority_is_rejected_but_legacy_local_authority_loads() {
        let remote = r#"{"trust_anchor":"https://workcell.example","authority_id":"server"}"#;
        let local = r#"{"trust_anchor":"caudra:local:v1","authority_id":"local"}"#;

        assert!(serde_json::from_str::<AuthorityIdentity>(remote).is_err());
        assert!(serde_json::from_str::<AuthorityIdentity>(local).is_ok());
    }
}
