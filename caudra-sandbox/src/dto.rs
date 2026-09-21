use caudra_config::sandbox::{
    CidrRule, DomainRule, MAX_NETWORK_RULES, Revision, SandboxName, TlsMode,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{net::IpAddr, ops::Not};

use crate::{Error, Result};

pub const PROTOCOL_VERSION: &str = "2026-07-28";
pub const TRANSFER_PROTOCOL: &str = "workcell-reviewed-v1";
pub const MAX_PAGE_SIZE: usize = 100;
pub const MAX_CATALOG_SIZE: usize = 4096;
pub const MIB_PER_GIB: u32 = 1024;
const MAX_ID_BYTES: usize = 256;
const POLICY_REVISION_PREFIX: &[u8] = b"e2b-libvirt-policy-v1\0";
pub const MAX_CPUS: u32 = 256;
pub const MAX_MIB: u32 = 1 << 20;
pub const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024 * 1024;
const MIN_MEMORY_MIB: u32 = 128;
const MAX_GUEST_ROOT_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    CreateFailed,
    Cancelled,
    InvalidRequest,
    Unauthenticated,
    IdempotencyConflict,
    StateConflict,
    LeaseExpired,
    Unsupported,
    PreconditionFailed,
    HistoryUnavailable,
    KeyExpired,
    Busy,
    JournalFull,
    Interrupted,
    Internal,
    TemplateNotFound,
    TemplateRevisionMismatch,
    TemplateIncompatible,
    TemplateIntegrity,
    LegacyTemplateUnverified,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub code: FailureCode,
    pub retryable: bool,
    pub outcome_unknown: bool,
}

#[derive(Debug, Deserialize)]
pub struct FailureEnvelope {
    pub error: Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub api_version: String,
    #[serde(rename = "ownerID")]
    pub owner_id: String,
    pub server_time: String,
    pub authentication: String,
    #[serde(rename = "templateID")]
    pub template_id: String,
    pub network_topology: String,
    #[serde(default)]
    pub tls_modes: Vec<TlsMode>,
    pub capabilities: Capabilities,
    pub limits: Limits,
    pub retention: DiscoveryRetention,
    pub idempotency_key: String,
    pub recovery: String,
    pub credential_scope: String,
    pub proxy_origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub idempotent_create: bool,
    pub operation_lookup: bool,
    pub conditional_mutations: bool,
    pub explicit_credentials: bool,
    pub persistent_disk: bool,
    pub memory_pause: bool,
    pub egress_policy: bool,
    pub cancel_create: bool,
    pub template_catalog: bool,
    pub conditional_template_create: bool,
    pub warm_start: bool,
    pub local_template_admin: bool,
    pub http_template_admin: bool,
    #[serde(default)]
    pub live_tls_mode_change: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_lease_seconds: u32,
    pub runtime_admission: u32,
    pub operation_journal_entries: u32,
    pub list_page_size: u32,
    pub resources: Resources,
    pub new_key_max_age_seconds: u32,
    pub new_key_future_skew_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryRetention {
    pub paused_disk_max_age_seconds: u64,
    pub operation_history_seconds: u64,
    pub history_starts_after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    pub cpu_count: u32,
    #[serde(rename = "memoryMB")]
    pub memory_mb: u32,
    #[serde(rename = "diskSizeMB")]
    pub disk_size_mb: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub mode: String,
    pub domains: Vec<String>,
    pub cidrs: Vec<String>,
}

impl Policy {
    pub(crate) fn canonical(&self) -> Result<Self> {
        self.validate()?;
        let mut policy = self.clone();
        policy.domains.sort();
        policy.domains.dedup();
        policy.cidrs.sort();
        policy.cidrs.dedup();
        Ok(policy)
    }

    pub(crate) fn revision(&self) -> Result<String> {
        let mut digest = Sha256::new();
        digest.update(POLICY_REVISION_PREFIX);
        digest.update(serde_json::to_vec(&self.canonical()?).map_err(|_| Error::Protocol)?);
        Ok(digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    pub fn validate(&self) -> Result<()> {
        if !matches!(self.mode.as_str(), "sni-only" | "mitm")
            || self.domains.len() > MAX_NETWORK_RULES
            || self.cidrs.len() > MAX_NETWORK_RULES
        {
            return Err(Error::Protocol);
        }
        for domain in &self.domains {
            if DomainRule::parse(domain)?.as_str() != domain {
                return Err(Error::Protocol);
            }
        }
        for cidr in &self.cidrs {
            if CidrRule::parse(cidr)?.as_str() != cidr {
                return Err(Error::Protocol);
            }
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        discovery: &Discovery,
        template: &Manifest,
        instance: Option<&Instance>,
    ) -> Result<()> {
        self.validate()?;
        let mode = if self.mode == "mitm" {
            TlsMode::Mitm
        } else {
            TlsMode::SniOnly
        };
        if !discovery.capabilities.egress_policy
            || !discovery.tls_modes.contains(&mode)
            || template.network_topology != "slirp-enforced"
            || (mode == TlsMode::Mitm && !template.guest_ca)
        {
            return Err(Error::TlsPolicy);
        }
        if let Some(instance) = instance {
            let previous = instance.egress.policy.as_ref().ok_or(Error::TlsPolicy)?;
            if !instance.egress.enforced
                || (previous.mode != self.mode
                    && (!discovery.capabilities.live_tls_mode_change
                        || instance.egress.effective_revision.as_ref()
                            != Some(&instance.egress.revision)
                        || (mode == TlsMode::Mitm && !instance.egress.guest_ca_ready)))
            {
                return Err(Error::TlsPolicy);
            }
        }
        Ok(())
    }

    pub fn test_destination(&self, destination: &str) -> Result<bool> {
        self.validate()?;
        if let Ok(address) = destination.parse::<IpAddr>() {
            return Ok(self.cidrs.iter().any(|cidr| cidr_contains(cidr, address)));
        }
        let host = DomainRule::parse(destination)?;
        if host.as_str().starts_with("*.") {
            return Err(Error::Protocol);
        }
        Ok(self.domains.iter().any(|rule| {
            rule == host.as_str()
                || rule.strip_prefix("*.").is_some_and(|suffix| {
                    host.as_str()
                        .strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
                })
        }))
    }
}

fn cidr_contains(cidr: &str, address: IpAddr) -> bool {
    let Some((network, prefix)) = cidr.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u32>() else {
        return false;
    };
    match (network.parse::<IpAddr>(), address) {
        (Ok(IpAddr::V4(network)), IpAddr::V4(address)) if prefix <= u32::BITS => {
            let mask = u32::MAX.checked_shl(u32::BITS - prefix).unwrap_or(0);
            u32::from(network) & mask == u32::from(address) & mask
        }
        (Ok(IpAddr::V6(network)), IpAddr::V6(address)) if prefix <= u128::BITS => {
            let mask = u128::MAX.checked_shl(u128::BITS - prefix).unwrap_or(0);
            u128::from(network) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Create {
    #[serde(rename = "templateID")]
    pub template_id: SandboxName,
    pub expected_template_revision: Revision,
    pub resources: Resources,
    pub lease_seconds: u32,
    pub persistent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<Policy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Expected {
    #[serde(rename = "expectedExecutionID")]
    pub expected_execution_id: String,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Creating,
    Running,
    Pausing,
    Paused,
    Resuming,
    CleanupPending,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkcellIdentity {
    #[serde(rename = "serverID")]
    pub server_id: String,
    #[serde(rename = "workspaceID")]
    pub workspace_id: String,
    pub workspace_generation: String,
    #[serde(rename = "projectID")]
    pub project_id: String,
    #[serde(rename = "principalID")]
    pub principal_id: String,
}

impl WorkcellIdentity {
    pub fn matches(&self, binding: &StoredWorkspaceBinding) -> bool {
        self.server_id == binding.server_id()
            && self.workspace_id == binding.workspace_id()
            && self.workspace_generation == binding.workspace_generation()
            && self.project_id == binding.project_key().as_str()
            && self.principal_id == binding.principal_id()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceTemplate {
    pub id: SandboxName,
    pub revision: String,
    pub image_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    pub paused_disk_max_age_seconds: u64,
    pub deadline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Egress {
    pub enforced: bool,
    pub revision: String,
    pub effective_revision: Option<String>,
    pub policy: Option<Policy>,
    #[serde(default, rename = "guestCAReady", skip_serializing_if = "Not::not")]
    pub guest_ca_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    #[serde(rename = "ownerID")]
    pub owner_id: String,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    #[serde(rename = "executionID")]
    pub execution_id: String,
    pub revision: u64,
    pub state: InstanceState,
    pub workspace_generation: String,
    pub expected_workcell: Option<WorkcellIdentity>,
    pub template: InstanceTemplate,
    pub resources: Resources,
    pub network_topology: String,
    pub persistent: bool,
    pub pause_unclean: bool,
    pub lease_deadline: Option<String>,
    pub retention: Retention,
    pub egress: Egress,
}

impl Instance {
    pub fn expected(&self) -> Expected {
        Expected {
            expected_execution_id: self.execution_id.clone(),
            expected_revision: self.revision,
        }
    }

    pub fn validate(&self, owner: &str) -> Result<()> {
        if self.owner_id != owner || self.revision == 0 {
            return Err(Error::Identity);
        }
        identifier(&self.sandbox_id)?;
        identifier(&self.execution_id)?;
        if let Some(identity) = &self.expected_workcell
            && (identity.server_id != self.sandbox_id
                || identity.workspace_id != self.sandbox_id
                || identity.project_id != self.sandbox_id
                || identity.principal_id != owner
                || identity.workspace_generation != self.workspace_generation
                || self.workspace_generation.is_empty())
        {
            return Err(Error::Identity);
        }
        for time in [&self.lease_deadline, &self.retention.deadline]
            .into_iter()
            .flatten()
        {
            timestamp(time)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Creating,
    Succeeded,
    Failed,
    CleanupPending,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    #[serde(rename = "operationID")]
    pub operation_id: String,
    #[serde(rename = "ownerID")]
    pub owner_id: String,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    #[serde(rename = "executionID")]
    pub execution_id: String,
    pub request_digest: String,
    pub status: OperationStatus,
    pub error_code: Option<FailureCode>,
    pub cancel_requested: bool,
    pub created_at: String,
    pub updated_at: String,
    pub history_deadline: Option<String>,
    pub instance: Option<Instance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub instance: Instance,
    traffic_access_token: String,
    pub mcp_path: String,
    pub files_path: String,
    pub credential_scope: String,
}

impl Credentials {
    pub fn take_token(self) -> String {
        self.traffic_access_token
    }
    pub(crate) fn valid_token(&self) -> bool {
        self.traffic_access_token.len() == 64
            && self
                .traffic_access_token
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkcellManifest {
    pub version: String,
    pub sha256: String,
    pub protocol_version: String,
    pub transfer_protocol: String,
    pub remote_workspace: bool,
    pub workspace_snapshots: bool,
    pub reviewed_transfer: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_root: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snapshot_root: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transfer_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildManifest {
    pub recipe: String,
    #[serde(rename = "recipeSHA256")]
    pub recipe_sha256: String,
    pub source_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub id: SandboxName,
    pub architecture: String,
    pub machine: String,
    pub minimum: Resources,
    pub defaults: Resources,
    pub network_topology: String,
    #[serde(default, rename = "guestCA", skip_serializing_if = "Not::not")]
    pub guest_ca: bool,
    pub workcell: WorkcellManifest,
    pub build: BuildManifest,
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        self.validate_metadata()?;
        let workcell = &self.workcell;
        if workcell.protocol_version != PROTOCOL_VERSION
            || workcell.transfer_protocol != TRANSFER_PROTOCOL
            || !workcell.remote_workspace
            || !workcell.reviewed_transfer
        {
            return Err(Error::ImageInput(
                "invalid Workcell protocol, transfer contract or feature declarations",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_metadata(&self) -> Result<()> {
        if self.schema_version != 1
            || self.architecture != "x86_64"
            || self.machine != "q35"
            || self
                .id
                .as_str()
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
        {
            return Err(Error::ImageInput(
                "schema 1, lowercase template ID and x86_64/q35 are required",
            ));
        }
        for (minimum, default, floor, maximum) in [
            (self.minimum.cpu_count, self.defaults.cpu_count, 1, MAX_CPUS),
            (
                self.minimum.memory_mb,
                self.defaults.memory_mb,
                MIN_MEMORY_MIB,
                MAX_MIB,
            ),
            (
                self.minimum.disk_size_mb,
                self.defaults.disk_size_mb,
                1,
                MAX_MIB,
            ),
        ] {
            if minimum < floor || default < minimum || default > maximum {
                return Err(Error::ImageInput(
                    "resources must satisfy supported minimum <= default <= maximum",
                ));
            }
        }
        if !matches!(
            self.network_topology.as_str(),
            "slirp-enforced" | "slirp-unrestricted" | "passt-unrestricted" | "managed-unrestricted"
        ) || (self.guest_ca && self.network_topology != "slirp-enforced")
        {
            return Err(Error::ImageInput(
                "unsupported topology or guest CA without enforced egress",
            ));
        }
        let workcell = &self.workcell;
        if workcell.version.is_empty()
            || workcell.version.len() > 64
            || !workcell.version.as_bytes()[0].is_ascii_alphanumeric()
            || !workcell
                .version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        {
            return Err(Error::ImageInput(
                "invalid Workcell version, protocol, transfer contract or feature declarations",
            ));
        }
        if !workcell.workspace_root.is_empty()
            || !workcell.snapshot_root.is_empty()
            || !workcell.transfer_root.is_empty()
        {
            guest_root(&workcell.workspace_root)?;
            if workcell.workspace_snapshots {
                guest_root(&workcell.snapshot_root)?;
                if roots_overlap(&workcell.workspace_root, &workcell.snapshot_root)
                    || roots_overlap(&workcell.snapshot_root, &workcell.workspace_root)
                {
                    return Err(Error::ImageInput(
                        "workspace and snapshot roots must not overlap",
                    ));
                }
            } else if !workcell.snapshot_root.is_empty() {
                return Err(Error::ImageInput(
                    "snapshot root requires workspace snapshots",
                ));
            }
            guest_root(&workcell.transfer_root)?;
            for root in [&workcell.workspace_root, &workcell.snapshot_root] {
                if !root.is_empty()
                    && (roots_overlap(root, &workcell.transfer_root)
                        || roots_overlap(&workcell.transfer_root, root))
                {
                    return Err(Error::ImageInput(
                        "reviewed transfer root must not overlap workspace or snapshot roots",
                    ));
                }
            }
        }
        if !matches!(
            self.build.recipe.as_str(),
            "import" | "base" | "egress" | "caudra"
        ) {
            return Err(Error::ImageInput("unsupported build recipe"));
        }
        for digest in [
            &workcell.sha256,
            &self.build.recipe_sha256,
            &self.build.source_revision,
        ] {
            if !digest.is_empty() {
                Revision::parse(digest)?;
            }
        }
        Ok(())
    }
}

fn guest_root(root: &str) -> Result<()> {
    if root.len() > MAX_GUEST_ROOT_BYTES
        || !root.starts_with('/')
        || root.contains('\\')
        || root.chars().any(char::is_control)
        || (root != "/"
            && root[1..]
                .split('/')
                .any(|part| matches!(part, "" | "." | "..")))
    {
        return Err(Error::ImageInput(
            "guest roots must be clean absolute paths",
        ));
    }
    Ok(())
}

fn roots_overlap(root: &str, other: &str) -> bool {
    root == "/"
        || root == other
        || other
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub format: String,
    pub file_size_bytes: u64,
    pub virtual_size_bytes: u64,
    pub cluster_size: u64,
    pub backing_policy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    #[serde(flatten)]
    pub manifest: Manifest,
    pub revision: Revision,
    #[serde(rename = "imageSHA256")]
    pub image_sha256: Revision,
    pub warm_start: bool,
    pub image: Image,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_after: String,
}

pub(crate) fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Error::Protocol);
    }
    Ok(())
}

pub(crate) fn timestamp(value: &str) -> Result<jiff::Timestamp> {
    value.parse().map_err(|_| Error::Protocol)
}

#[cfg(test)]
mod tests {
    use super::Policy;
    use test_case::test_case;

    const DOMAIN: &str = "example.test";
    const WILDCARD: &str = "*.sub.example.test";

    #[test_case("example.test", Some(true); "exact_domain")]
    #[test_case("child.sub.example.test", Some(true); "wildcard_child")]
    #[test_case("sub.example.test", Some(false); "wildcard_not_apex")]
    #[test_case("evil-example.test", Some(false); "suffix_is_not_domain")]
    #[test_case("203.0.113.9", Some(true); "ipv4_cidr")]
    #[test_case("203.0.112.9", Some(false); "ipv4_outside")]
    #[test_case("2001:db8::1", Some(true); "ipv6_cidr")]
    #[test_case("2001:db9::1", Some(false); "ipv6_outside")]
    #[test_case("https://example.test/private", None; "url_is_not_rule_test")]
    #[test_case("*.example.test", None; "test_requires_destination")]
    fn destination_test_is_pure_rule_evaluation(destination: &str, expected: Option<bool>) {
        let policy = Policy {
            mode: "sni-only".into(),
            domains: vec![DOMAIN.into(), WILDCARD.into()],
            cidrs: vec!["203.0.113.0/24".into(), "2001:db8::/32".into()],
        };
        assert_eq!(policy.test_destination(destination).ok(), expected);
    }
}
