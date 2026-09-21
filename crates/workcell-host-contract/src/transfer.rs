use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    ContractVersion, Identifier, PrepareResponse, ResourceId, Revision, WorkspacePath,
    WorkspaceRequestBinding, deserialize_bounded_vec,
};

pub const TRANSFER_STAGE_METHOD: &str = "ai.workcell/transfer/stage";
pub const TRANSFER_SEAL_METHOD: &str = "ai.workcell/transfer/seal";
pub const TRANSFER_RELEASE_METHOD: &str = "ai.workcell/transfer/release";
pub const TRANSFER_STAT_METHOD: &str = "ai.workcell/transfer/stat";
pub const TRANSFER_DOWNLOAD_METHOD: &str = "ai.workcell/transfer/download";
pub const TRANSFER_PREPARE_METHOD: &str = "ai.workcell/transfer/preparePublication";
pub const TRANSFER_STATUS_METHOD: &str = "ai.workcell/transfer/publicationStatus";
pub const TRANSFER_INVENTORY_METHOD: &str = "ai.workcell/transfer/inventory";
pub const MAX_TRANSFER_INVENTORY_ENTRIES: usize = 4096;
pub const MAX_TRANSFER_INVENTORY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_TRANSFER_DEPTH: usize = 32;
pub const MAX_TRANSFER_EXCLUDES: usize = 256;
pub const MAX_TRANSFER_EXCLUDE_BYTES: usize = 512;
pub const TRANSFER_PUBLICATION_CONTRACT_ID: &str = "ai.workcell/transfer-publication";
pub const MAX_TRANSFER_STAGES: u32 = 32;
pub const MAX_TRANSFER_RESERVED_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_TRANSFER_CONCURRENCY: u32 = 4;
pub const TRANSFER_TTL_MS: u64 = 10 * 60 * 1000;
pub const TRANSFER_IO_TIMEOUT_MS: u64 = 60 * 1000;
pub const MAX_TRANSFER_JOURNALS: u32 = 256;
pub const MAX_TRANSFER_JOURNAL_BYTES: u64 = 32 * 1024;
pub const MAX_TRANSFER_JOURNAL_STORAGE_BYTES: u64 =
    MAX_TRANSFER_JOURNAL_BYTES * MAX_TRANSFER_JOURNALS as u64;
pub const TRANSFER_OUTCOME_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
pub const TRANSFER_STREAM_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewedTransferCapability {
    pub version: ContractVersion,
    pub private_staging: bool,
    pub sealed_publication: bool,
    pub conditional_download: bool,
    pub single_range: bool,
    pub durable_outcomes: bool,
    pub creates_directories: bool,
    /// Root-only descriptor traversal, no symlinks, mount crossings (including bind mounts),
    /// special-file reads, or nested repositories. Unsupported hosts refuse inventory.
    pub safe_inventory: bool,
    pub atomic_replace_against_external_writers: bool,
    pub limits: ReviewedTransferLimits,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewedTransferLimits {
    pub max_file_bytes: u64,
    pub max_stages: u32,
    pub max_reserved_bytes: u64,
    pub max_concurrent_io: u32,
    pub stage_ttl_ms: u64,
    pub io_timeout_ms: u64,
    pub max_journals: u32,
    pub max_journal_bytes: u64,
    pub max_journal_storage_bytes: u64,
    pub outcome_retention_ms: u64,
    pub stream_buffer_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferStageRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub size_bytes: u64,
    /// Lowercase `sha256:` followed by exactly 64 hexadecimal digits.
    pub digest: Revision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferStageResponse {
    pub version: ContractVersion,
    pub stage_id: Identifier,
    /// Same-origin relative route, not an authorization capability.
    pub upload_path: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferStageSelector {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub stage_id: Identifier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferSealResponse {
    pub version: ContractVersion,
    pub stage_id: Identifier,
    pub digest: Revision,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferReleaseResponse {
    pub version: ContractVersion,
    pub released: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", tag = "kind")]
pub enum TransferPrecondition {
    MustNotExist {},
    Revision { revision: Revision },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferMode {
    Regular,
    Executable,
}

impl TransferMode {
    #[must_use]
    pub const fn bits(&self) -> u32 {
        match self {
            Self::Regular => 0o644,
            Self::Executable => 0o755,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferPrepareRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    /// Client-chosen idempotency key. Never retry an unknown or indeterminate outcome blindly.
    pub publication_id: Identifier,
    pub stage_id: Identifier,
    pub digest: Revision,
    pub size_bytes: u64,
    pub path: WorkspacePath,
    /// Exact missing ancestors, shallowest first. Each is a conditional, reviewed creation.
    #[serde(deserialize_with = "deserialize_directories")]
    pub create_directories: Vec<WorkspacePath>,
    pub precondition: TransferPrecondition,
    pub mode: TransferMode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferPrepareResponse {
    pub version: ContractVersion,
    pub publication_id: Identifier,
    pub operation: PrepareResponse,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferStatRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferFile {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    /// Identity, mode, timestamps and content, distinct from the byte digest.
    pub revision: Revision,
    pub digest: Revision,
    pub size_bytes: u64,
    pub mode: TransferMode,
    #[serde(deserialize_with = "deserialize_directory_identities")]
    pub created_directories: Vec<(WorkspacePath, ResourceId)>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferStatResponse {
    pub version: ContractVersion,
    pub file: TransferFile,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferDownloadRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
    pub revision: Revision,
    pub digest: Revision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferDownloadResponse {
    pub version: ContractVersion,
    pub download_id: Identifier,
    pub download_path: String,
    pub file: TransferFile,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferStatusRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub publication_id: Identifier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferPublicationState {
    Prepared,
    Publishing,
    Completed,
    Failed,
    Cancelled,
    Indeterminate,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferStatusResponse {
    pub version: ContractVersion,
    pub publication_id: Identifier,
    pub state: TransferPublicationState,
    #[serde(deserialize_with = "Option::deserialize")]
    pub preparation_id: Option<Identifier>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub invocation_id: Option<Identifier>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub request_digest: Option<Revision>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub file: Option<TransferFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferInventoryRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub inspect: Option<WorkspacePath>,
    #[serde(default)]
    pub policy: TransferInventoryPolicy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferInventoryPolicy {
    #[serde(deserialize_with = "deserialize_excludes")]
    pub excludes: Vec<String>,
    pub respect_gitignore: bool,
}

impl Default for TransferInventoryPolicy {
    fn default() -> Self {
        Self {
            excludes: Vec::new(),
            respect_gitignore: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferNodeKind {
    File,
    Directory,
    Symlink,
    Mount,
    Special,
    NestedRepository,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferInventoryNode {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub revision: Revision,
    pub kind: TransferNodeKind,
    pub size_bytes: Option<u64>,
    pub ignored: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferInspection {
    pub path: WorkspacePath,
    pub node: Option<TransferInventoryNode>,
    pub ignored: bool,
}

/// Bounded live enumeration, not a filesystem snapshot. Boundary/excluded directories appear
/// once without descendants; complete covers the eligible tree and all effective ignore inputs.
/// Only in-root per-directory .gitignore inputs apply; global Git configuration and
/// .git/info/exclude are never read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransferInventoryResponse {
    pub version: ContractVersion,
    #[serde(deserialize_with = "deserialize_inventory_nodes")]
    pub entries: Vec<TransferInventoryNode>,
    pub revision: Revision,
    pub ignore_digest: Revision,
    pub complete: bool,
    pub inspection: Option<TransferInspection>,
}

fn deserialize_directories<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<WorkspacePath>, D::Error> {
    deserialize_bounded_vec(deserializer, MAX_TRANSFER_DEPTH, "createDirectories")
}

fn deserialize_directory_identities<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(WorkspacePath, ResourceId)>, D::Error> {
    deserialize_bounded_vec(deserializer, MAX_TRANSFER_DEPTH, "createdDirectories")
}

fn deserialize_inventory_nodes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TransferInventoryNode>, D::Error> {
    deserialize_bounded_vec(deserializer, MAX_TRANSFER_INVENTORY_ENTRIES, "entries")
}

fn deserialize_excludes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    let excludes: Vec<String> =
        deserialize_bounded_vec(deserializer, MAX_TRANSFER_EXCLUDES, "excludes")?;
    if excludes
        .iter()
        .any(|pattern| pattern.len() > MAX_TRANSFER_EXCLUDE_BYTES)
    {
        return Err(serde::de::Error::custom(
            "transfer exclude exceeds its bound",
        ));
    }
    Ok(excludes)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_TRANSFER_EXCLUDE_BYTES, MAX_TRANSFER_EXCLUDES, TransferInventoryPolicy, TransferMode,
        TransferPrecondition,
    };
    use serde_json::json;

    #[test]
    fn publication_preconditions_and_metadata_never_ignore_unknown_policy() {
        assert!(
            serde_json::from_value::<TransferPrecondition>(json!({"kind":"mustNotExist"})).is_ok()
        );
        assert!(
            serde_json::from_value::<TransferPrecondition>(
                json!({"kind":"mustNotExist","overwrite":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<TransferPrecondition>(json!({"kind":"revision"})).is_err()
        );
        assert!(serde_json::from_value::<TransferMode>(json!("executable")).is_ok());
        assert!(serde_json::from_value::<TransferMode>(json!("setuid")).is_err());
        assert!(serde_json::from_value::<TransferMode>(json!(0o777)).is_err());
    }

    #[test]
    fn inventory_policy_cannot_negotiate_safety_or_exceed_filter_bounds() {
        assert!(
            serde_json::from_value::<TransferInventoryPolicy>(
                json!({"excludes": [], "respectGitignore": false, "followSymlinks": true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<TransferInventoryPolicy>(
                json!({"excludes": vec!["x"; MAX_TRANSFER_EXCLUDES + 1], "respectGitignore": true})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<TransferInventoryPolicy>(json!({"excludes": ["x".repeat(MAX_TRANSFER_EXCLUDE_BYTES + 1)], "respectGitignore": true})).is_err());
        assert!(
            serde_json::from_value::<TransferInventoryPolicy>(
                json!({"excludes": [], "respectGitignore": false})
            )
            .is_ok()
        );
    }
}
