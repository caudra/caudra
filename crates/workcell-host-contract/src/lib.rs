#![forbid(unsafe_code)]

use std::{fmt, marker::PhantomData, ops::Deref};

mod transfer;
pub use transfer::*;

use serde::{Deserialize, Deserializer, Serialize, de::SeqAccess};
use serde_json::Value;

pub const EXTENSION_ID: &str = "ai.workcell/remote-host";
pub const PREPARE_METHOD: &str = "ai.workcell/prepare";
pub const EXECUTE_METHOD: &str = "ai.workcell/execute";
pub const RELEASE_METHOD: &str = "ai.workcell/release";
pub const STATUS_METHOD: &str = "ai.workcell/status";
pub const CANCEL_METHOD: &str = "ai.workcell/cancel";
pub const RESOLVE_DIRECTORY_METHOD: &str = "ai.workcell/resolve-directory";
pub const STAT_METHOD: &str = "ai.workcell/stat";
pub const LIST_METHOD: &str = "ai.workcell/list";
pub const READ_TEXT_METHOD: &str = "ai.workcell/read-text";
pub const SEARCH_TEXT_METHOD: &str = "ai.workcell/search-text";
pub const WATCH_OPEN_METHOD: &str = "ai.workcell/watch-open";
pub const WATCH_POLL_METHOD: &str = "ai.workcell/watch-poll";
pub const WATCH_CLOSE_METHOD: &str = "ai.workcell/watch-close";
pub const DISCOVER_PROJECT_ASSETS_METHOD: &str = "ai.workcell/discover-project-assets";
pub const READ_PROJECT_ASSET_METHOD: &str = "ai.workcell/read-project-asset";
pub const PREPARE_MUTATION_METHOD: &str = "ai.workcell/prepare-mutation";
pub const PREPARE_EXEC_METHOD: &str = "ai.workcell/prepare-exec";
pub const SCM_DISCOVER_METHOD: &str = "ai.workcell/scm-discover";
pub const SCM_STATUS_METHOD: &str = "ai.workcell/scm-status";
pub const SCM_LOG_METHOD: &str = "ai.workcell/scm-log";
pub const SCM_DIFF_METHOD: &str = "ai.workcell/scm-diff";
pub const SCM_READ_SIDE_METHOD: &str = "ai.workcell/scm-read-side";
pub const SCM_PREPARE_MUTATION_METHOD: &str = "ai.workcell/scm-prepare-mutation";
pub const SNAPSHOT_CAPTURE_METHOD: &str = "ai.workcell/snapshot-capture";
pub const SNAPSHOT_PREPARE_CAPTURE_METHOD: &str = "ai.workcell/snapshot-prepare-capture";
pub const SNAPSHOT_CHECKPOINT_METHOD: &str = "ai.workcell/snapshot-checkpoint";
pub const SNAPSHOT_INSPECT_METHOD: &str = "ai.workcell/snapshot-inspect";
pub const SNAPSHOT_STATUS_METHOD: &str = "ai.workcell/snapshot-status";
pub const SNAPSHOT_PREPARE_RESTORE_METHOD: &str = "ai.workcell/snapshot-prepare-restore";
pub const SNAPSHOT_PREPARE_UNREVERT_METHOD: &str = "ai.workcell/snapshot-prepare-unrevert";
pub const SNAPSHOT_ACKNOWLEDGE_METHOD: &str = "ai.workcell/snapshot-acknowledge";
pub const SNAPSHOT_PREPARE_CLEANUP_METHOD: &str = "ai.workcell/snapshot-prepare-cleanup";
pub const WORKSPACE_MUTATION_CONTRACT_ID: &str = "workspace.mutation.v1";
pub const DIRECT_EXEC_CONTRACT_ID: &str = "workspace.exec.v1";
pub const SCM_MUTATION_CONTRACT_ID: &str = "workspace.scm.mutation.v1";
pub const SNAPSHOT_RESTORE_CONTRACT_ID: &str = "workspace.snapshot.restore.v2";
pub const SNAPSHOT_CAPTURE_CONTRACT_ID: &str = "workcell.snapshot.capture.v1";
pub const SNAPSHOT_UNREVERT_CONTRACT_ID: &str = "workspace.snapshot.unrevert.v2";
pub const SNAPSHOT_CLEANUP_CONTRACT_ID: &str = "workspace.snapshot.cleanup.v2";
pub const MAX_ARGUMENT_BYTES: usize = 1_048_576;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_DISPLAY_TEXT_BYTES: usize = 65_536;
pub const MAX_ERROR_TEXT_BYTES: usize = 16_384;
pub const MAX_PROGRESS_CHUNK_BYTES: usize = 16_384;
pub const MAX_TOOL_RESULT_TEXT_BYTES: usize = 1_048_576;
pub const MAX_TOOL_RESULT_STRUCTURED_BYTES: usize = 16 * 1_024 * 1_024;
pub const MAX_TOOL_RESULT_CONTENT: usize = 64;
pub const MAX_RESOURCE_INTENTS: usize = 128;
pub const MAX_RESOURCE_SCOPE_DEPTH: usize = 128;
pub const MAX_PROGRESS_EVENTS: usize = 256;
pub const MAX_WORKSPACE_PATH_BYTES: usize = 4_096;
pub const MAX_CURSOR_BYTES: usize = 128;
pub const MAX_SEARCH_PATTERN_BYTES: usize = 4_096;
pub const MAX_INCLUDE_PATTERN_BYTES: usize = 4_096;
pub const MAX_MUTATION_CONTENT_BYTES: usize = 5 * 1_024 * 1_024;
pub const MAX_MUTATIONS: usize = 32;
pub const MAX_COMMAND_BYTES: usize = 64 * 1_024;
pub const MAX_PAGE_SIZE: u32 = 500;
pub const MAX_TEXT_READ_BYTES: u32 = 64 * 1_024;
pub const MAX_WORKSPACE_LIST_ENTRIES: u32 = 50_000;
pub const MAX_WORKSPACE_LIST_RETAINED_BYTES: u64 = 16 * 1_024 * 1_024;
pub const MAX_WORKSPACE_LIST_HASH_BYTES: u64 = 64 * 1_024 * 1_024;
pub const MAX_SEARCH_MATCH_TEXT_BYTES: usize = 16 * 1_024;
pub const MAX_WATCH_SUBSCRIPTIONS: usize = 8;
pub const MAX_WATCH_RETAINED_EVENTS: usize = 256;
pub const MAX_WATCH_LIFETIME_EVENTS: usize = 4_096;
pub const MAX_WATCH_RETAINED_BYTES: usize = 512 * 1_024;
pub const MAX_WATCH_POLL_EVENTS: u32 = 128;
pub const MAX_WATCH_POLL_BYTES: u32 = 256 * 1_024;
pub const MAX_WATCH_WAIT_MS: u64 = 1_000;
pub const WATCH_SUBSCRIPTION_TTL_MS: u64 = 120_000;
pub const MAX_PROJECT_ASSETS: usize = 256;
pub const MAX_PROJECT_ASSET_READ_BYTES: u32 = 64 * 1_024;
pub const PROJECT_ASSET_MANIFEST_VERSION: &str = "project-assets.v1";
pub const MAX_SCM_PATHS: usize = MAX_RESOURCE_INTENTS - 1;
pub const MAX_SCM_STATUS_ENTRIES: u32 = 500;
pub const MAX_SCM_STATUS_PATHS: u32 = 10_000;
pub const MAX_SCM_LOG_ENTRIES: u32 = 200;
pub const MAX_SCM_LOG_COMMITS: u32 = 10_000;
pub const MAX_SCM_CONFIG_BYTES: u64 = 1_024 * 1_024;
pub const MAX_SCM_COMMIT_BYTES: u64 = 1_024 * 1_024;
pub const MAX_SCM_LOG_SCAN_BYTES: u64 = 16 * 1_024 * 1_024;
pub const MAX_SCM_SHALLOW_BYTES: usize = 1_024 * 1_024;
pub const MAX_SCM_SHALLOW_COMMITS: usize = MAX_SCM_LOG_COMMITS as usize;
pub const MAX_SCM_DIFF_LINES: u32 = 2_000;
pub const MAX_SCM_DIFF_BYTES: u32 = 512 * 1_024;
pub const MAX_SCM_DIFF_FILES: u32 = 500;
pub const MAX_SCM_DIFF_SCAN_BYTES: u32 = 16 * 1_024 * 1_024;
pub const MAX_SCM_DIFF_PARSED_LINES: u32 = 20_000;
pub const MAX_SCM_SIDE_LINES: u32 = 4_000;
pub const MAX_SCM_SIDE_BYTES: u32 = 512 * 1_024;
pub const MAX_SCM_TEXT_BYTES: usize = MAX_SCM_SIDE_BYTES as usize;
pub const MAX_SNAPSHOT_FILES: usize = 50_000;
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 100 * 1_024 * 1_024;
pub const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 512 * 1_024 * 1_024;
pub const MAX_SNAPSHOT_CAPTURE_ENTRIES: usize = 250_000;
pub const MAX_SNAPSHOT_CAPTURE_PATH_BYTES: u64 = 64 * 1_024 * 1_024;
pub const MAX_SNAPSHOT_COUNT: usize = 256;
pub const MAX_SNAPSHOT_STORAGE_BYTES: u64 = 2 * 1_024 * 1_024 * 1_024;
pub const MAX_SNAPSHOT_CLEANUP: usize = 128;
pub const MAX_SNAPSHOT_JOURNALS: usize = 256;
pub const MAX_SNAPSHOT_PREVIEW_CHANGES: usize = MAX_PAGE_SIZE as usize;
pub const MAX_SNAPSHOT_SKIPPED_SAMPLES: usize = 32;
pub const MAX_SNAPSHOT_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContractVersion {
    V1,
}

macro_rules! bounded_opaque_string {
    ($name:ident, $limit:expr) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
                let value = value.into();
                validate_opaque_string(stringify!($name), &value, $limit)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn retained_bytes(&self) -> usize {
                std::mem::size_of::<Self>().saturating_add(self.0.capacity())
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.as_str()
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

macro_rules! bounded_text {
    ($name:ident, $limit:expr, $allow_empty:expr, $allow_nul:expr) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
                let value = value.into();
                validate_text(stringify!($name), &value, $limit, $allow_empty, $allow_nul)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn retained_bytes(&self) -> usize {
                std::mem::size_of::<Self>().saturating_add(self.0.capacity())
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.as_str()
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

bounded_opaque_string!(Identifier, MAX_ID_BYTES);
bounded_opaque_string!(Revision, MAX_ID_BYTES);
bounded_opaque_string!(ToolName, MAX_ID_BYTES);
bounded_opaque_string!(ResourceId, MAX_ID_BYTES);
bounded_opaque_string!(Cursor, MAX_CURSOR_BYTES);
bounded_text!(DisplayText, MAX_DISPLAY_TEXT_BYTES, false, false);
bounded_text!(ErrorText, MAX_ERROR_TEXT_BYTES, false, false);
bounded_text!(ProgressChunkText, MAX_PROGRESS_CHUNK_BYTES, true, true);
bounded_text!(ToolResultText, MAX_TOOL_RESULT_TEXT_BYTES, true, true);
bounded_text!(SearchPattern, MAX_SEARCH_PATTERN_BYTES, false, false);
bounded_text!(IncludePattern, MAX_INCLUDE_PATTERN_BYTES, false, false);
bounded_text!(MutationContent, MAX_MUTATION_CONTENT_BYTES, true, true);
bounded_text!(CommandText, MAX_COMMAND_BYTES, false, true);
bounded_text!(ScmText, MAX_SCM_TEXT_BYTES, true, false);
bounded_text!(
    ProjectAssetContent,
    MAX_PROJECT_ASSET_READ_BYTES as usize,
    true,
    true
);

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkspacePath(String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DirectoryNavigation(WorkspacePath);

impl DirectoryNavigation {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let path = WorkspacePath::new(value)?;
        if path.as_str().split('/').any(str::is_empty) {
            return Err(ValidationError::new(
                "DirectoryNavigation",
                "contains an empty component",
            ));
        }
        Ok(Self(path))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> Deserialize<'de> for DirectoryNavigation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl WorkspacePath {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        validate_text(
            "WorkspacePath",
            &value,
            MAX_WORKSPACE_PATH_BYTES,
            false,
            false,
        )?;
        if value.starts_with('/') || value.contains('\\') {
            return Err(ValidationError::new(
                "WorkspacePath",
                "must use relative POSIX syntax",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.0.capacity())
    }
}

impl Deref for WorkspacePath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl<'de> Deserialize<'de> for WorkspacePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    field: &'static str,
    reason: &'static str,
}

impl ValidationError {
    const fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.field, self.reason)
    }
}

impl std::error::Error for ValidationError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteHostConfiguration {
    pub server_id: Identifier,
    pub workspace_id: Identifier,
    pub workspace_generation: Identifier,
    pub root_project_id: Identifier,
    pub principal_id: Identifier,
}

impl RemoteHostConfiguration {
    pub fn new(
        server_id: String,
        workspace_id: String,
        workspace_generation: String,
        root_project_id: String,
        principal_id: String,
    ) -> Result<Self, ValidationError> {
        Ok(Self {
            server_id: Identifier::new(server_id)?,
            workspace_id: Identifier::new(workspace_id)?,
            workspace_generation: Identifier::new(workspace_generation)?,
            root_project_id: Identifier::new(root_project_id)?,
            principal_id: Identifier::new(principal_id)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HostBinding {
    pub server_id: Identifier,
    pub instance_id: Identifier,
    pub workspace_id: Identifier,
    pub workspace_generation: Identifier,
    pub root_project_id: Identifier,
    pub principal_id: Identifier,
    pub cwd_handle: ResourceId,
    pub catalog_revision: Revision,
    pub policy_revision: Revision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContractBinding {
    pub id: Identifier,
    pub version: Identifier,
    pub result_version: Identifier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperationBinding {
    pub host: HostBinding,
    pub contract: ContractBinding,
    pub argument_digest: Revision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteHostDescriptor {
    pub version: ContractVersion,
    pub server_id: Identifier,
    pub workspace_id: Identifier,
    pub workspace_generation: Identifier,
    pub root_project_id: Identifier,
    pub principal_id: Identifier,
    pub instance_id: Identifier,
    pub resource_namespace_version: Identifier,
    pub path_style: Identifier,
    pub revisions: RemoteHostRevisions,
    pub cwd: RemoteHostCwd,
    pub capabilities: RemoteHostCapabilities,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteHostRevisions {
    pub execution_environment: Revision,
    pub catalog: Revision,
    pub policy: Revision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteHostCwd {
    pub handle: ResourceId,
    pub display_path: DisplayText,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteHostCapabilities {
    pub tool_catalog: RemoteHostToolCapability,
    pub tool_execution: RemoteHostToolCapability,
    pub execution_environment: Option<RemoteHostToolCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_transfer: Option<ReviewedTransferCapability>,
    pub operations: Option<RemoteOperationCapability>,
    pub workspace: Option<WorkspaceCapability>,
    pub watch: Option<WorkspaceWatchCapability>,
    pub project_assets: Option<ProjectAssetCapability>,
    pub workspace_mutation: Option<WorkspaceMutationCapability>,
    pub direct_exec: Option<DirectExecCapability>,
    pub scm: Option<ScmCapability>,
    pub snapshots: Option<WorkspaceSnapshotCapability>,
    pub control_plane: bool,
    #[serde(deserialize_with = "deserialize_control_plane_missing")]
    pub control_plane_missing: Vec<Identifier>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceSnapshotCapability {
    pub version: ContractVersion,
    pub methods: WorkspaceSnapshotMethods,
    pub limits: WorkspaceSnapshotLimits,
    pub atomic_across_files: bool,
    pub durable_per_file_journal: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceSnapshotMethods {
    pub capture: bool,
    #[serde(default)]
    pub prepare_capture: bool,
    #[serde(default)]
    pub checkpoint: bool,
    pub inspect: bool,
    pub status: bool,
    pub prepare_restore: bool,
    pub prepare_unrevert: bool,
    pub acknowledge: bool,
    pub prepare_cleanup: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceSnapshotLimits {
    pub max_files: u32,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_capture_entries: u32,
    pub max_capture_path_bytes: u64,
    pub max_snapshots: u32,
    pub max_storage_bytes: u64,
    pub max_concurrent_captures: u32,
    pub max_cleanup_checkpoints: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceWatchCapability {
    pub version: ContractVersion,
    pub methods: WorkspaceWatchMethods,
    pub limits: WorkspaceWatchLimits,
    pub recursive: bool,
    pub exact_rename_pairing: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceWatchMethods {
    pub open: bool,
    pub poll: bool,
    pub close: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceWatchLimits {
    pub max_subscriptions: u32,
    pub max_retained_events: u32,
    pub max_retained_bytes: u64,
    pub max_lifetime_events: u64,
    pub max_poll_events: u32,
    pub max_poll_bytes: u32,
    pub max_wait_ms: u64,
    pub subscription_ttl_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectAssetCapability {
    pub version: ContractVersion,
    pub manifest_version: Identifier,
    pub methods: ProjectAssetMethods,
    pub limits: ProjectAssetLimits,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectAssetMethods {
    pub discover: bool,
    pub read: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectAssetLimits {
    pub max_assets: u32,
    pub max_read_bytes: u32,
    pub max_path_bytes: u32,
    pub max_discovery_entries: u32,
    pub max_discovery_retained_bytes: u64,
    pub max_discovery_hash_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceCapability {
    pub version: ContractVersion,
    pub methods: WorkspaceMethods,
    pub limits: WorkspaceLimits,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceMethods {
    pub resolve_directory: bool,
    pub stat: bool,
    pub list: bool,
    pub read_text: bool,
    pub search_text: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceLimits {
    pub max_path_bytes: u32,
    pub max_page_size: u32,
    pub max_text_read_bytes: u32,
    pub max_search_pattern_bytes: u32,
    pub max_cursor_bytes: u32,
    pub max_list_entries: u32,
    pub max_list_retained_bytes: u64,
    pub max_list_hash_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceMutationCapability {
    pub version: ContractVersion,
    pub prepared: bool,
    pub max_mutations: u32,
    pub max_content_bytes: u64,
    pub atomic_across_files: bool,
    pub rollback_on_failure: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DirectExecCapability {
    pub version: ContractVersion,
    pub prepared: bool,
    pub interactive: bool,
    pub max_command_bytes: u32,
    pub max_timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmCapability {
    pub version: ContractVersion,
    pub methods: ScmMethods,
    pub limits: ScmLimits,
    pub prepared_mutations: bool,
    pub discard_untracked: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmMethods {
    pub discover: bool,
    pub status: bool,
    pub log: bool,
    pub diff: bool,
    pub read_side: bool,
    pub stage: bool,
    pub unstage: bool,
    pub discard: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmLimits {
    pub max_concurrent_operations: u32,
    pub max_paths: u32,
    pub max_status_entries: u32,
    pub max_status_paths: u32,
    pub max_config_bytes: u64,
    pub max_log_entries: u32,
    pub max_log_commits: u32,
    pub max_commit_bytes: u64,
    pub max_log_scan_bytes: u64,
    pub max_diff_lines: u32,
    pub max_diff_bytes: u32,
    pub max_diff_files: u32,
    pub max_diff_scan_bytes: u32,
    pub max_diff_parsed_lines: u32,
    pub max_side_lines: u32,
    pub max_side_bytes: u32,
    pub max_cursor_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmRepositoryRevisions {
    pub repository: Revision,
    pub head: Revision,
    pub index: Revision,
    pub worktree: Revision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmRepository {
    pub handle: ResourceId,
    pub resource_id: ResourceId,
    pub root: WorkspacePath,
    pub identity: Revision,
    pub revisions: ScmRepositoryRevisions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmDiscoverRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmDiscoverResponse {
    pub version: ContractVersion,
    pub repository: ScmRepository,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScmChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmStatusEntry {
    pub path: WorkspacePath,
    pub staged: Option<ScmChangeKind>,
    pub unstaged: Option<ScmChangeKind>,
    pub untracked: bool,
    pub conflicted: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmStatusRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub repository_handle: ResourceId,
    pub page_size: u32,
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmStatusResponse {
    pub version: ContractVersion,
    pub revisions: ScmRepositoryRevisions,
    pub revision: Revision,
    #[serde(deserialize_with = "deserialize_scm_status_entries")]
    pub entries: Vec<ScmStatusEntry>,
    pub next_cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmCommit {
    pub id: Revision,
    pub parents: Vec<Revision>,
    pub author_name: ScmText,
    pub author_email: ScmText,
    pub committed_unix_seconds: i64,
    pub summary: ScmText,
    /// The message past its subject line. Optional so a host that predates the
    /// field still deserializes: `None` means the server did not report one,
    /// which a reader must not confuse with a commit whose message is a subject
    /// and nothing else.
    pub body: Option<ScmText>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmLogRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub repository_handle: ResourceId,
    pub page_size: u32,
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmLogResponse {
    pub version: ContractVersion,
    pub head_revision: Revision,
    pub revision: Revision,
    #[serde(deserialize_with = "deserialize_scm_log_entries")]
    pub commits: Vec<ScmCommit>,
    pub truncated: bool,
    pub next_cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", tag = "kind")]
pub enum ScmDiffTarget {
    Staged,
    Unstaged,
    Tree { base: Revision, target: Revision },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScmDiffLineKind {
    File,
    Context,
    Addition,
    Deletion,
    Binary,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmDiffLine {
    pub path: WorkspacePath,
    pub kind: ScmDiffLineKind,
    pub change: Option<ScmChangeKind>,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: ScmText,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmDiffRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub repository_handle: ResourceId,
    pub target: ScmDiffTarget,
    pub path: Option<WorkspacePath>,
    pub max_lines: u32,
    pub max_bytes: u32,
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmDiffResponse {
    pub version: ContractVersion,
    pub repository_revision: Revision,
    pub revision: Revision,
    #[serde(deserialize_with = "deserialize_scm_diff_lines")]
    pub lines: Vec<ScmDiffLine>,
    pub truncated: bool,
    pub next_cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", tag = "kind")]
pub enum ScmSide {
    Head,
    Index,
    Worktree,
    Commit { revision: Revision },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmReadSideRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub repository_handle: ResourceId,
    pub path: WorkspacePath,
    pub side: ScmSide,
    pub start_line: u32,
    pub max_lines: u32,
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmReadSideResponse {
    pub version: ContractVersion,
    pub repository_revision: Revision,
    pub resource_id: ResourceId,
    pub revision: Revision,
    pub path: WorkspacePath,
    pub side: ScmSide,
    pub content: ScmText,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub truncated: bool,
    pub next_start_line: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", tag = "kind")]
pub enum ScmMutation {
    Stage {
        #[serde(deserialize_with = "deserialize_scm_paths")]
        paths: Vec<WorkspacePath>,
    },
    Unstage {
        #[serde(deserialize_with = "deserialize_scm_paths")]
        paths: Vec<WorkspacePath>,
    },
    Discard {
        #[serde(deserialize_with = "deserialize_scm_paths")]
        paths: Vec<WorkspacePath>,
    },
}

impl ScmMutation {
    #[must_use]
    pub fn paths(&self) -> &[WorkspacePath] {
        match self {
            Self::Stage { paths } | Self::Unstage { paths } | Self::Discard { paths } => paths,
        }
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        let paths = match self {
            Self::Stage { paths } | Self::Unstage { paths } | Self::Discard { paths } => paths,
        };
        std::mem::size_of::<Self>()
            .saturating_add(
                paths
                    .capacity()
                    .saturating_mul(std::mem::size_of::<WorkspacePath>()),
            )
            .saturating_add(
                paths
                    .iter()
                    .map(WorkspacePath::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmPrepareMutationRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub repository_handle: ResourceId,
    pub mutation: ScmMutation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmMutationPreview {
    pub mutation: ScmMutation,
    pub repository_identity: Revision,
    pub revisions: ScmRepositoryRevisions,
    #[serde(deserialize_with = "deserialize_scm_status_entries")]
    pub entries: Vec<ScmStatusEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmPrepareMutationResponse {
    pub version: ContractVersion,
    pub operation: PrepareResponse,
    pub preview: ScmMutationPreview,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScmMutationResponse {
    pub version: ContractVersion,
    pub mutation: ScmMutation,
    pub revisions: ScmRepositoryRevisions,
}

/// Per-capture ceilings a client may lower below the host's advertised limits.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotCaptureLimits {
    pub max_files: u32,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

/// Captures the directory named by `binding.cwd_handle`, the session's working directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotCaptureRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub checkpoint_id: Identifier,
    pub limits: SnapshotCaptureLimits,
}

pub type SnapshotPrepareCaptureRequest = SnapshotCaptureRequest;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotCheckpointRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub checkpoint_id: Identifier,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotState {
    Complete,
    Corrupt,
}

/// The limit or quota a snapshot operation reached, carried in `limit_exceeded` and
/// `quota_exceeded` error data beside its `maximum` where the limit has one.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotLimit {
    Files,
    TotalBytes,
    CaptureEntries,
    CapturePathBytes,
    Depth,
    IgnoreRules,
    ManifestBytes,
    PreparedBytes,
    Snapshots,
    Checkpoints,
    StorageBytes,
    Journals,
}

impl SnapshotLimit {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::TotalBytes => "totalBytes",
            Self::CaptureEntries => "captureEntries",
            Self::CapturePathBytes => "capturePathBytes",
            Self::Depth => "depth",
            Self::IgnoreRules => "ignoreRules",
            Self::ManifestBytes => "manifestBytes",
            Self::PreparedBytes => "preparedBytes",
            Self::Snapshots => "snapshots",
            Self::Checkpoints => "checkpoints",
            Self::StorageBytes => "storageBytes",
            Self::Journals => "journals",
        }
    }
}

impl fmt::Display for SnapshotLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a capture left an entry out. A restore never touches an entry left out of either side.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotSkipReason {
    NestedRepository,
    Mount,
    Special,
    Oversized,
    Unreadable,
    Unstable,
    Unrepresentable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotSkippedEntry {
    /// Lossy for an unrepresentable name, so it is for display only.
    pub path: DisplayText,
    pub reason: SnapshotSkipReason,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotSkipped {
    pub nested_repositories: u32,
    pub mounts: u32,
    pub special_files: u32,
    pub oversized_files: u32,
    pub unreadable_entries: u32,
    pub unstable_files: u32,
    pub unrepresentable_names: u32,
    #[serde(deserialize_with = "deserialize_snapshot_skipped_samples")]
    pub samples: Vec<SnapshotSkippedEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotSummary {
    pub snapshot_id: Identifier,
    pub checkpoint_id: Option<Identifier>,
    pub state: SnapshotState,
    pub manifest_revision: Revision,
    pub scope: WorkspacePath,
    pub file_count: u32,
    pub total_bytes: u64,
    pub skipped: SnapshotSkipped,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotCaptureResponse {
    pub version: ContractVersion,
    pub snapshot: SnapshotSummary,
    pub reused_checkpoint: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotInspectRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub snapshot_id: Identifier,
    pub page_size: u32,
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotEntryKind {
    File,
    /// Captured as the link itself: `digest` covers the raw target and it is never followed.
    Symlink,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotFile {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub kind: SnapshotEntryKind,
    pub digest: Revision,
    pub mode: u32,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotInspectResponse {
    pub version: ContractVersion,
    pub snapshot: SnapshotSummary,
    #[serde(deserialize_with = "deserialize_snapshot_files")]
    pub files: Vec<SnapshotFile>,
    #[serde(deserialize_with = "deserialize_snapshot_paths")]
    pub exclusions: Vec<WorkspacePath>,
    pub next_cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotPrepareRestoreRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub snapshot_id: Identifier,
    /// The capture the workspace is believed to match. Only paths that differ between it and the
    /// target are restored, and each must still match this side when it is replaced.
    pub source_snapshot_id: Identifier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotPrepareUnrevertRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub restore_id: Identifier,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotChangeKind {
    Create,
    Replace,
    Delete,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotChange {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub kind: SnapshotChangeKind,
    pub current_revision: Option<Revision>,
    pub target_revision: Option<Revision>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotChangeCounts {
    pub create: u32,
    pub replace: u32,
    pub delete: u32,
    pub conflict: u32,
    /// Paths that differ between the two captures but already match the target.
    pub unchanged: u32,
    pub created_directories: u32,
}

/// `changes` and `created_directories` are bounded samples, conflicts first; `counts` is complete.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotRestorePreview {
    pub restore_id: Identifier,
    pub target_snapshot_id: Identifier,
    pub source_snapshot_id: Identifier,
    pub counts: SnapshotChangeCounts,
    #[serde(deserialize_with = "deserialize_snapshot_changes")]
    pub changes: Vec<SnapshotChange>,
    #[serde(deserialize_with = "deserialize_snapshot_restore_directories")]
    pub created_directories: Vec<WorkspacePath>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotPrepareRestoreResponse {
    pub version: ContractVersion,
    pub operation: PrepareResponse,
    pub preview: SnapshotRestorePreview,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotRestoreState {
    Publishing,
    Completed,
    Partial,
    Indeterminate,
    Acknowledged,
    Reverted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotRestoreStatus {
    pub restore_id: Identifier,
    pub state: SnapshotRestoreState,
    pub target_snapshot_id: Identifier,
    pub source_snapshot_id: Identifier,
    pub applied_files: u32,
    pub total_files: u32,
    pub acknowledgement_required: bool,
    pub reconciliation_required: bool,
    pub unrevert_of: Option<Identifier>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotStatusRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub restore_id: Identifier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotStatusResponse {
    pub version: ContractVersion,
    pub restore: SnapshotRestoreStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotAcknowledgeRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub restore_id: Identifier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotAcknowledgeResponse {
    pub version: ContractVersion,
    pub restore: SnapshotRestoreStatus,
}

/// Deletes checkpoints, never snapshots: a content-addressed snapshot may back checkpoints of other
/// sessions. Snapshots and blobs nothing references any more are collected afterwards.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotPrepareCleanupRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    #[serde(deserialize_with = "deserialize_checkpoint_ids")]
    pub checkpoint_ids: Vec<Identifier>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotCleanupPreview {
    #[serde(deserialize_with = "deserialize_checkpoint_ids")]
    pub checkpoint_ids: Vec<Identifier>,
    #[serde(deserialize_with = "deserialize_checkpoint_ids")]
    pub missing_checkpoint_ids: Vec<Identifier>,
    pub reclaimable_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotPrepareCleanupResponse {
    pub version: ContractVersion,
    pub operation: PrepareResponse,
    pub preview: SnapshotCleanupPreview,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SnapshotCleanupResponse {
    pub version: ContractVersion,
    #[serde(deserialize_with = "deserialize_checkpoint_ids")]
    pub deleted_checkpoint_ids: Vec<Identifier>,
    pub deleted_snapshots: u32,
    pub deleted_blobs: u32,
    pub reclaimed_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteHostToolCapability {
    pub version: ContractVersion,
    pub limits: RemoteHostToolLimits,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteHostToolLimits {
    pub max_request_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteOperationCapability {
    pub version: ContractVersion,
    pub exact_preparation: bool,
    pub methods: RemoteOperationMethods,
    pub limits: RemoteOperationLimits,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteOperationMethods {
    pub prepare: bool,
    pub execute: bool,
    pub release: bool,
    pub status: bool,
    pub cancel: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteOperationLimits {
    pub preparation_ttl_ms: u64,
    pub max_preparations: u32,
    pub max_operations: u32,
    pub max_ledger_bytes: u64,
    pub max_argument_bytes: u64,
    pub max_resource_intents: u32,
    pub max_progress_events: u32,
    pub max_progress_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceRequestBinding {
    pub host: HostBinding,
    pub cwd_handle: ResourceId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResolveDirectoryRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: DirectoryNavigation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceDirectory {
    pub handle: ResourceId,
    pub resource_id: ResourceId,
    pub revision: Revision,
    pub display_path: WorkspacePath,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResolveDirectoryResponse {
    pub version: ContractVersion,
    pub directory: WorkspaceDirectory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StatRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceEntry {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub revision: Revision,
    pub kind: WorkspaceEntryKind,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StatResponse {
    pub version: ContractVersion,
    pub entry: WorkspaceEntry,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ListRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
    pub recursive: bool,
    pub page_size: u32,
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ListResponse {
    pub version: ContractVersion,
    pub revision: Revision,
    #[serde(deserialize_with = "deserialize_workspace_entries")]
    pub entries: Vec<WorkspaceEntry>,
    pub truncated: bool,
    pub next_cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TextRange {
    pub start_line: u32,
    pub end_line: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReadTextRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
    pub range: Option<TextRange>,
    #[serde(default)]
    pub byte_offset: u64,
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReadTextResponse {
    pub version: ContractVersion,
    pub resource_id: ResourceId,
    pub revision: Revision,
    pub path: WorkspacePath,
    #[serde(deserialize_with = "deserialize_read_text")]
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub start_byte: u64,
    pub end_byte: u64,
    pub truncated: bool,
    pub next_byte_offset: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SearchTextRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
    pub pattern: SearchPattern,
    pub include: Option<IncludePattern>,
    pub page_size: u32,
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TextSearchMatch {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub revision: Revision,
    pub line: u32,
    #[serde(deserialize_with = "deserialize_search_match_text")]
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SearchTextResponse {
    pub version: ContractVersion,
    pub revision: Revision,
    #[serde(deserialize_with = "deserialize_search_matches")]
    pub matches: Vec<TextSearchMatch>,
    pub files_scanned: u32,
    pub files_listed: u32,
    pub truncated: bool,
    pub next_cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchOpenRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
    pub recursive: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WatchState {
    Current,
    FullResync,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WatchResyncReason {
    CursorInvalid,
    InstanceChanged,
    Overflow,
    BackendError,
    RetentionLost,
    SubscriptionExpired,
    SubscriptionClosed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchOpenResponse {
    pub version: ContractVersion,
    pub subscription_id: Identifier,
    pub state: WatchState,
    pub cursor: Cursor,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchPollRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub subscription_id: Identifier,
    pub cursor: Cursor,
    pub max_events: u32,
    pub max_bytes: u32,
    pub wait_ms: u64,
}

impl WatchPollRequest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.max_events == 0 || self.max_events > MAX_WATCH_POLL_EVENTS {
            return Err(ValidationError::new(
                "maxEvents",
                "must be within the advertised limit",
            ));
        }
        if self.max_bytes == 0 || self.max_bytes > MAX_WATCH_POLL_BYTES {
            return Err(ValidationError::new(
                "maxBytes",
                "must be within the advertised limit",
            ));
        }
        if self.wait_ms > MAX_WATCH_WAIT_MS {
            return Err(ValidationError::new(
                "waitMs",
                "must be within the advertised limit",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchEventKind {
    Create,
    Modify,
    Remove,
    Rescan,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchEvent {
    pub sequence: u64,
    pub kind: WatchEventKind,
    pub path: WorkspacePath,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchPollResponse {
    pub version: ContractVersion,
    pub subscription_id: Identifier,
    pub state: WatchState,
    pub resync_reason: Option<WatchResyncReason>,
    pub first_retained_sequence: Option<u64>,
    pub next_sequence: u64,
    #[serde(deserialize_with = "deserialize_watch_events")]
    pub events: Vec<WatchEvent>,
    pub next_cursor: Option<Cursor>,
    pub expires_at_unix_ms: Option<u64>,
}

impl WatchPollResponse {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.events.len() > MAX_WATCH_POLL_EVENTS as usize {
            return Err(ValidationError::new("events", "exceed the count limit"));
        }
        if !self
            .events
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence)
            || self
                .events
                .last()
                .is_some_and(|event| event.sequence >= self.next_sequence)
        {
            return Err(ValidationError::new("events", "are not strictly ordered"));
        }
        match self.state {
            WatchState::Current
                if self.resync_reason.is_none()
                    && self.next_cursor.is_some()
                    && self.expires_at_unix_ms.is_some() =>
            {
                Ok(())
            }
            WatchState::FullResync
                if self.resync_reason.is_some()
                    && self.events.is_empty()
                    && self.next_cursor.is_none()
                    && self.expires_at_unix_ms.is_none() =>
            {
                Ok(())
            }
            _ => Err(ValidationError::new(
                "state",
                "does not match cursor and resync metadata",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchCloseRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub subscription_id: Identifier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchCloseResponse {
    pub version: ContractVersion,
    pub subscription_id: Identifier,
    pub closed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DiscoverProjectAssetsRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectAssetKind {
    Instructions,
    Skill,
    Command,
    Workflow,
    Permissions,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectAssetTrust {
    Declarative,
    ClientApprovalRequired,
    MixedReviewRequired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectAsset {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub revision: Revision,
    pub kind: ProjectAssetKind,
    pub trust: ProjectAssetTrust,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectAssetManifest {
    pub version: Identifier,
    pub revision: Revision,
    #[serde(deserialize_with = "deserialize_project_assets")]
    pub assets: Vec<ProjectAsset>,
    /// Paths discovery could not read, so nothing beneath them was
    /// discovered. At most [`MAX_PROJECT_ASSETS`] are named.
    #[serde(deserialize_with = "deserialize_unreadable_project_paths")]
    pub unreadable: Vec<WorkspacePath>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DiscoverProjectAssetsResponse {
    pub version: ContractVersion,
    pub manifest: ProjectAssetManifest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReadProjectAssetRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub path: WorkspacePath,
    pub expected_revision: Revision,
    pub max_bytes: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectAssetEncoding {
    Utf8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReadProjectAssetResponse {
    pub version: ContractVersion,
    pub asset: ProjectAsset,
    pub encoding: ProjectAssetEncoding,
    pub content: ProjectAssetContent,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PrepareMutationRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    #[serde(deserialize_with = "deserialize_mutations")]
    pub mutations: Vec<WorkspaceMutation>,
}

impl PrepareMutationRequest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.mutations.is_empty() || self.mutations.len() > MAX_MUTATIONS {
            return Err(ValidationError::new(
                "mutations",
                "must contain a bounded non-empty batch",
            ));
        }
        let content_bytes = self
            .mutations
            .iter()
            .map(WorkspaceMutation::content_bytes)
            .sum::<usize>();
        if content_bytes > MAX_MUTATION_CONTENT_BYTES {
            return Err(ValidationError::new(
                "mutations",
                "exceed the aggregate content byte limit",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum WorkspaceMutation {
    Create {
        path: WorkspacePath,
        content: MutationContent,
    },
    Write {
        path: WorkspacePath,
        content: MutationContent,
        expected_revision: Revision,
    },
    Mkdir {
        path: WorkspacePath,
    },
    Rename {
        from: WorkspacePath,
        to: WorkspacePath,
        expected_revision: Revision,
    },
    Delete {
        path: WorkspacePath,
        expected_revision: Revision,
    },
}

impl WorkspaceMutation {
    fn content_bytes(&self) -> usize {
        match self {
            Self::Create { content, .. } | Self::Write { content, .. } => content.len(),
            Self::Mkdir { .. } | Self::Rename { .. } | Self::Delete { .. } => 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceMutationResult {
    pub kind: WorkspaceMutationKind,
    pub path: WorkspacePath,
    pub destination: Option<WorkspacePath>,
    pub revision: Option<Revision>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMutationKind {
    Create,
    Write,
    Mkdir,
    Rename,
    Delete,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceMutationResponse {
    pub version: ContractVersion,
    pub committed: bool,
    pub rolled_back: bool,
    pub atomic_across_files: bool,
    #[serde(deserialize_with = "deserialize_mutation_results")]
    pub results: Vec<WorkspaceMutationResult>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DirectExecOptions {
    pub command: CommandText,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PrepareExecRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub binding: WorkspaceRequestBinding,
    pub options: DirectExecOptions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PrepareRequest {
    pub version: ContractVersion,
    pub host: HostBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_handle: Option<ResourceId>,
    pub tool: ToolName,
    pub contract: ContractBinding,
    #[serde(deserialize_with = "deserialize_arguments")]
    pub arguments: Value,
}

impl PrepareRequest {
    pub fn validate(&self, max_argument_bytes: u64) -> Result<(), ValidationError> {
        let bytes = serde_json::to_vec(&self.arguments)
            .map_err(|_| ValidationError::new("arguments", "cannot be encoded"))?;
        let limit = usize::try_from(max_argument_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_ARGUMENT_BYTES);
        if bytes.len() > limit {
            return Err(ValidationError::new("arguments", "exceed the byte limit"));
        }
        if !self.arguments.is_object() {
            return Err(ValidationError::new("arguments", "must be an object"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperationSelector {
    pub preparation_id: Identifier,
    pub invocation_id: Option<Identifier>,
    pub host: HostBinding,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExecuteRequest {
    pub version: ContractVersion,
    pub preparation_id: Identifier,
    pub invocation_id: Identifier,
    pub host: HostBinding,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StatusRequest {
    pub version: ContractVersion,
    /// Exclusive replay cursor; absent means all retained progress, not just new events.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_after_sequence"
    )]
    pub after_sequence: Option<u64>,
    #[serde(flatten)]
    pub selector: OperationSelector,
}

fn deserialize_after_sequence<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    let cursor = Option::<u64>::deserialize(deserializer)?;
    if cursor == Some(u64::MAX) {
        return Err(serde::de::Error::custom(
            "afterSequence exceeds the sequence limit",
        ));
    }
    Ok(cursor)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReleaseRequest {
    pub version: ContractVersion,
    #[serde(flatten)]
    pub selector: OperationSelector,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CancelRequest {
    pub version: ContractVersion,
    pub preparation_id: Identifier,
    pub invocation_id: Identifier,
    pub host: HostBinding,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationKind {
    Inspect,
    Read,
    Search,
    Mutate,
    Execute,
    Transfer,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceAccess {
    Inspect,
    Read,
    Search,
    Traverse,
    Write,
    ReadWrite,
    Delete,
    Execute,
    Connect,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResourceIntent {
    pub resource_id: ResourceId,
    #[serde(deserialize_with = "deserialize_resource_scope")]
    pub scope: Vec<ResourceId>,
    pub display: DisplayText,
    pub access: ResourceAccess,
    pub revision: Option<Revision>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperationIntent {
    pub kind: OperationKind,
    pub mutating: bool,
    #[serde(deserialize_with = "deserialize_resource_intents")]
    pub resources: Vec<ResourceIntent>,
}

impl ResourceIntent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.scope.is_empty()
            || self.scope.len() > MAX_RESOURCE_SCOPE_DEPTH
            || self.scope.last() != Some(&self.resource_id)
            || self
                .scope
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != self.scope.len()
        {
            return Err(ValidationError::new("scope", "invalid resource ancestry"));
        }
        Ok(())
    }
}

impl OperationIntent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        for resource in &self.resources {
            resource.validate()?;
        }
        if self.resources.len() > MAX_RESOURCE_INTENTS {
            return Err(ValidationError::new("resources", "exceed the count limit"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PrepareResponse {
    pub version: ContractVersion,
    pub preparation_id: Identifier,
    pub expires_at_unix_ms: u64,
    pub binding: OperationBinding,
    pub intent: OperationIntent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationState {
    NeverSeen,
    Prepared,
    Running,
    Completed,
    Failed,
    Cancelled,
    Forgotten,
    Indeterminate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OutcomeKind {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultVersion {
    V1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolResultContent {
    Text { text: ToolResultText },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ToolResultEnvelope {
    pub version: ToolResultVersion,
    #[serde(deserialize_with = "deserialize_tool_result_content")]
    pub content: Vec<ToolResultContent>,
    #[serde(default, deserialize_with = "deserialize_optional_bounded_value")]
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

impl ToolResultEnvelope {
    pub fn new(
        content: Vec<ToolResultContent>,
        structured_content: Option<Value>,
        is_error: bool,
    ) -> Result<Self, ValidationError> {
        if content.len() > MAX_TOOL_RESULT_CONTENT {
            return Err(ValidationError::new("content", "exceeds the count limit"));
        }
        if let Some(value) = &structured_content {
            validate_value_bytes("structuredContent", value, MAX_TOOL_RESULT_STRUCTURED_BYTES)?;
        }
        Ok(Self {
            version: ToolResultVersion::V1,
            content,
            structured_content,
            is_error,
        })
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.content
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ToolResultContent>()),
            )
            .saturating_add(
                self.content
                    .iter()
                    .map(|content| match content {
                        ToolResultContent::Text { text } => text.retained_bytes(),
                    })
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                self.structured_content
                    .as_ref()
                    .map_or(0, retained_json_bytes),
            )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StructuredOutcome {
    pub kind: OutcomeKind,
    pub side_effects_possible: bool,
    pub result: Option<ToolResultEnvelope>,
    pub error: Option<OperationError>,
}

impl StructuredOutcome {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.result
                    .as_ref()
                    .map_or(0, ToolResultEnvelope::retained_bytes),
            )
            .saturating_add(self.error.as_ref().map_or(0, |error| {
                std::mem::size_of::<OperationError>()
                    .saturating_add(error.code.retained_bytes())
                    .saturating_add(error.message.retained_bytes())
            }))
    }
}

fn retained_json_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => std::mem::size_of::<Value>(),
        Value::String(value) => std::mem::size_of::<Value>().saturating_add(value.capacity()),
        Value::Array(values) => std::mem::size_of::<Value>()
            .saturating_add(
                values
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Value>()),
            )
            .saturating_add(
                values
                    .iter()
                    .map(retained_json_bytes)
                    .fold(0, usize::saturating_add),
            ),
        Value::Object(values) => std::mem::size_of::<Value>()
            .saturating_add(
                values
                    .len()
                    .saturating_mul(std::mem::size_of::<(String, Value)>().saturating_mul(2)),
            )
            .saturating_add(
                values
                    .iter()
                    .map(|(key, value)| key.capacity().saturating_add(retained_json_bytes(value)))
                    .fold(0, usize::saturating_add),
            ),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperationError {
    pub code: Identifier,
    pub message: ErrorText,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProgressEvent {
    pub execution_id: Identifier,
    pub sequence: u64,
    pub kind: Identifier,
    pub chunk: ProgressChunkText,
}

impl ProgressEvent {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.execution_id.retained_bytes())
            .saturating_add(self.kind.retained_bytes())
            .saturating_add(self.chunk.retained_bytes())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProgressMetadata {
    /// Ledger retention floor, which need not appear in a cursor-filtered response.
    pub first_retained_sequence: Option<u64>,
    /// Lifetime high-water mark plus one, unchanged by replay filtering.
    pub next_sequence: u64,
    /// Sticky disclosure of producer loss or retention eviction, not cursor filtering.
    pub gap_before_first: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StatusResponse {
    pub version: ContractVersion,
    pub state: OperationState,
    pub preparation_id: Identifier,
    pub invocation_id: Option<Identifier>,
    pub execution_id: Option<Identifier>,
    pub expires_at_unix_ms: Option<u64>,
    pub binding: Option<OperationBinding>,
    pub outcome: Option<StructuredOutcome>,
    /// Wall clock of the newest tombstone this ledger has evicted, if any.
    /// Tombstones are evicted oldest first and an operation cannot be forgotten
    /// before it was dispatched, so a caller whose operation was dispatched
    /// after this instant can trust `NeverSeen` to mean the operation never ran.
    pub tombstones_evicted_through_unix_ms: Option<u64>,
    pub progress_metadata: ProgressMetadata,
    #[serde(deserialize_with = "deserialize_progress_events")]
    pub progress: Vec<ProgressEvent>,
}

impl StatusResponse {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.progress.len() > MAX_PROGRESS_EVENTS {
            return Err(ValidationError::new("progress", "exceeds the count limit"));
        }
        if self.progress_metadata.next_sequence == 0
            || self
                .progress_metadata
                .first_retained_sequence
                .is_some_and(|first| first == 0 || first >= self.progress_metadata.next_sequence)
            || self.progress.first().is_some_and(|event| {
                self.progress_metadata
                    .first_retained_sequence
                    .is_none_or(|first| first > event.sequence)
            })
            || self
                .progress
                .windows(2)
                .any(|events| events[0].sequence >= events[1].sequence)
        {
            return Err(ValidationError::new(
                "progressMetadata",
                "does not match retained progress",
            ));
        }
        if self
            .progress
            .last()
            .is_some_and(|event| event.sequence >= self.progress_metadata.next_sequence)
        {
            return Err(ValidationError::new(
                "progressMetadata",
                "next sequence does not follow retained progress",
            ));
        }
        let complete = self
            .progress_metadata
            .first_retained_sequence
            .map_or(self.progress_metadata.next_sequence == 1, |first| {
                first == 1
            })
            && self
                .progress
                .windows(2)
                .all(|events| events[0].sequence.saturating_add(1) == events[1].sequence);
        if !complete && !self.progress_metadata.gap_before_first {
            return Err(ValidationError::new(
                "progressMetadata",
                "must disclose a progress gap",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReleaseResponse {
    pub version: ContractVersion,
    pub state: OperationState,
    pub released: bool,
    /// See [`StatusResponse::tombstones_evicted_through_unix_ms`].
    pub tombstones_evicted_through_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CancelResponse {
    pub version: ContractVersion,
    pub state: OperationState,
    pub cancellation_requested: bool,
}

fn validate_opaque_string(
    field: &'static str,
    value: &str,
    limit: usize,
) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::new(field, "must not be empty"));
    }
    if value.len() > limit {
        return Err(ValidationError::new(field, "exceeds the byte limit"));
    }
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(ValidationError::new(
            field,
            "must be a trimmed printable value",
        ));
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    limit: usize,
    allow_empty: bool,
    allow_nul: bool,
) -> Result<(), ValidationError> {
    if !allow_empty && value.is_empty() {
        return Err(ValidationError::new(field, "must not be empty"));
    }
    if value.len() > limit {
        return Err(ValidationError::new(field, "exceeds the byte limit"));
    }
    if !allow_nul && value.contains('\0') {
        return Err(ValidationError::new(field, "must not contain NUL"));
    }
    Ok(())
}

fn validate_value_bytes(
    field: &'static str,
    value: &Value,
    limit: usize,
) -> Result<(), ValidationError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| ValidationError::new(field, "cannot be encoded"))?;
    if bytes.len() > limit {
        return Err(ValidationError::new(field, "exceeds the byte limit"));
    }
    Ok(())
}

fn deserialize_arguments<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(serde::de::Error::custom("arguments must be an object"));
    }
    validate_value_bytes("arguments", &value, MAX_ARGUMENT_BYTES)
        .map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn deserialize_optional_bounded_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    if let Some(value) = &value {
        validate_value_bytes("structuredContent", value, MAX_TOOL_RESULT_STRUCTURED_BYTES)
            .map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn deserialize_resource_intents<'de, D>(deserializer: D) -> Result<Vec<ResourceIntent>, D::Error>
where
    D: Deserializer<'de>,
{
    let resources: Vec<ResourceIntent> =
        deserialize_bounded_vec(deserializer, MAX_RESOURCE_INTENTS, "resources")?;
    for resource in &resources {
        resource.validate().map_err(serde::de::Error::custom)?;
    }
    Ok(resources)
}

fn deserialize_mutations<'de, D>(deserializer: D) -> Result<Vec<WorkspaceMutation>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_MUTATIONS, "mutations")
}

fn deserialize_scm_paths<'de, D>(deserializer: D) -> Result<Vec<WorkspacePath>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_SCM_PATHS, "paths")
}

fn deserialize_scm_status_entries<'de, D>(deserializer: D) -> Result<Vec<ScmStatusEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_SCM_STATUS_ENTRIES as usize, "entries")
}

fn deserialize_scm_log_entries<'de, D>(deserializer: D) -> Result<Vec<ScmCommit>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_SCM_LOG_ENTRIES as usize, "commits")
}

fn deserialize_scm_diff_lines<'de, D>(deserializer: D) -> Result<Vec<ScmDiffLine>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_SCM_DIFF_LINES as usize, "lines")
}

fn deserialize_snapshot_files<'de, D>(deserializer: D) -> Result<Vec<SnapshotFile>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_PAGE_SIZE as usize, "files")
}

fn deserialize_snapshot_skipped_samples<'de, D>(
    deserializer: D,
) -> Result<Vec<SnapshotSkippedEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_SNAPSHOT_SKIPPED_SAMPLES, "samples")
}

fn deserialize_snapshot_changes<'de, D>(deserializer: D) -> Result<Vec<SnapshotChange>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_SNAPSHOT_PREVIEW_CHANGES, "changes")
}

fn deserialize_snapshot_restore_directories<'de, D>(
    deserializer: D,
) -> Result<Vec<WorkspacePath>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(
        deserializer,
        MAX_SNAPSHOT_PREVIEW_CHANGES,
        "createdDirectories",
    )
}

fn deserialize_checkpoint_ids<'de, D>(deserializer: D) -> Result<Vec<Identifier>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_SNAPSHOT_CLEANUP, "checkpointIds")
}

fn deserialize_snapshot_paths<'de, D>(deserializer: D) -> Result<Vec<WorkspacePath>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, 32, "exclusions")
}

fn deserialize_control_plane_missing<'de, D>(deserializer: D) -> Result<Vec<Identifier>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, 16, "controlPlaneMissing")
}

fn deserialize_workspace_entries<'de, D>(deserializer: D) -> Result<Vec<WorkspaceEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_PAGE_SIZE as usize, "entries")
}

fn deserialize_search_matches<'de, D>(deserializer: D) -> Result<Vec<TextSearchMatch>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_PAGE_SIZE as usize, "matches")
}

fn deserialize_watch_events<'de, D>(deserializer: D) -> Result<Vec<WatchEvent>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_WATCH_POLL_EVENTS as usize, "events")
}

fn deserialize_project_assets<'de, D>(deserializer: D) -> Result<Vec<ProjectAsset>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_PROJECT_ASSETS, "assets")
}

fn deserialize_unreadable_project_paths<'de, D>(
    deserializer: D,
) -> Result<Vec<WorkspacePath>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_PROJECT_ASSETS, "unreadable")
}

fn deserialize_mutation_results<'de, D>(
    deserializer: D,
) -> Result<Vec<WorkspaceMutationResult>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_MUTATIONS, "results")
}

fn deserialize_read_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, MAX_TEXT_READ_BYTES as usize, "text")
}

fn deserialize_search_match_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, MAX_SEARCH_MATCH_TEXT_BYTES, "text")
}

fn deserialize_bounded_string<'de, D>(
    deserializer: D,
    limit: usize,
    field: &'static str,
) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() > limit {
        return Err(serde::de::Error::custom(format_args!(
            "{field} exceeds the byte limit"
        )));
    }
    Ok(value)
}

fn deserialize_progress_events<'de, D>(deserializer: D) -> Result<Vec<ProgressEvent>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_PROGRESS_EVENTS, "progress")
}

fn deserialize_tool_result_content<'de, D>(
    deserializer: D,
) -> Result<Vec<ToolResultContent>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_TOOL_RESULT_CONTENT, "content")
}

fn deserialize_bounded_vec<'de, D, T>(
    deserializer: D,
    limit: usize,
    field: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedVecVisitor<T> {
        limit: usize,
        field: &'static str,
        marker: PhantomData<T>,
    }

    impl<'de, T> serde::de::Visitor<'de> for BoundedVecVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {} {} entries", self.limit, self.field)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(self.limit));
            while values.len() < self.limit {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(format_args!(
                    "{} exceeds the count limit",
                    self.field
                )));
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedVecVisitor {
        limit,
        field,
        marker: PhantomData,
    })
}

fn deserialize_resource_scope<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ResourceId>, D::Error> {
    let scope: Vec<ResourceId> =
        deserialize_bounded_vec(deserializer, MAX_RESOURCE_SCOPE_DEPTH, "scope")?;
    if scope.is_empty()
        || scope.iter().collect::<std::collections::HashSet<_>>().len() != scope.len()
    {
        return Err(serde::de::Error::custom("invalid resource ancestry"));
    }
    Ok(scope)
}

#[cfg(test)]
mod tests {
    #[test]
    fn resource_ancestry_rejects_missing_duplicate_overdeep_and_wrong_leaf() {
        let valid = serde_json::json!({"kind":"read","mutating":false,"resources":[{
            "resourceId":"leaf", "scope":["root","parent","leaf"],
            "display":"display only", "access":"read", "revision":null
        }]});
        assert!(serde_json::from_value::<super::OperationIntent>(valid.clone()).is_ok());
        for scope in [
            serde_json::json!([]),
            serde_json::json!(["root", "root", "leaf"]),
            serde_json::json!(["root", "other"]),
            serde_json::json!(vec!["leaf"; super::MAX_RESOURCE_SCOPE_DEPTH + 1]),
        ] {
            let mut invalid = valid.clone();
            invalid["resources"][0]["scope"] = scope;
            assert!(serde_json::from_value::<super::OperationIntent>(invalid).is_err());
        }
        let mut missing = valid;
        missing["resources"][0]
            .as_object_mut()
            .unwrap()
            .remove("scope");
        assert!(serde_json::from_value::<super::OperationIntent>(missing).is_err());
    }

    use serde_json::json;

    use super::*;

    #[test]
    fn capture_wire_contracts_use_the_common_preparation_and_lookup_has_no_work_inputs() {
        assert_eq!(
            SNAPSHOT_PREPARE_CAPTURE_METHOD,
            "ai.workcell/snapshot-prepare-capture"
        );
        assert_eq!(
            SNAPSHOT_CHECKPOINT_METHOD,
            "ai.workcell/snapshot-checkpoint"
        );
        assert_eq!(SNAPSHOT_CAPTURE_CONTRACT_ID, "workcell.snapshot.capture.v1");
        let lookup = json!({
            "version":"v1", "host":binding(), "cwdHandle":"cwd", "checkpointId":"checkpoint"
        });
        let checkpoint: SnapshotCheckpointRequest = serde_json::from_value(lookup.clone()).unwrap();
        assert_eq!(serde_json::to_value(checkpoint).unwrap(), lookup);
        let mut capture = lookup.clone();
        capture["limits"] = json!({"maxFiles":100,"maxFileBytes":1024,"maxTotalBytes":4096});
        let prepared: SnapshotPrepareCaptureRequest =
            serde_json::from_value(capture.clone()).unwrap();
        assert_eq!(serde_json::to_value(prepared).unwrap(), capture);
        assert!(serde_json::from_value::<SnapshotCheckpointRequest>(capture).is_err());
        assert!(serde_json::from_value::<SnapshotPrepareCaptureRequest>(lookup).is_err());
        let methods = json!({
            "capture":true,"prepareCapture":true,"checkpoint":true,"inspect":true,"status":true,
            "prepareRestore":true,"prepareUnrevert":true,"acknowledge":true,"prepareCleanup":true
        });
        let advertised: WorkspaceSnapshotMethods = serde_json::from_value(methods.clone()).unwrap();
        assert!(advertised.prepare_capture && advertised.checkpoint);
        assert_eq!(serde_json::to_value(advertised).unwrap(), methods);
    }

    #[test]
    fn external_strings_and_unknown_fields_are_rejected() {
        assert!(Identifier::new("").is_err());
        assert!(Identifier::new("x".repeat(MAX_ID_BYTES + 1)).is_err());
        assert!(
            serde_json::from_value::<RemoteHostConfiguration>(json!({
                "serverId": "server",
                "workspaceId": "workspace",
                "workspaceGeneration": "generation",
                "rootProjectId": "project",
                "principalId": "principal",
                "tenantId": "not-supported"
            }))
            .is_err()
        );
        assert!(
            RemoteHostConfiguration::new(
                "server".into(),
                "workspace".into(),
                "x".repeat(MAX_ID_BYTES + 1),
                "project".into(),
                "principal".into(),
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<RemoteHostConfiguration>(json!({
                "serverId": "server",
                "workspaceId": "workspace",
                "rootProjectId": "project",
                "principalId": "principal"
            }))
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(binding()).unwrap(),
            json!({
                "serverId": "server",
                "instanceId": "instance",
                "workspaceId": "workspace",
                "workspaceGeneration": "generation",
                "rootProjectId": "project",
                "principalId": "principal",
                "cwdHandle": "sha256:cwd",
                "catalogRevision": "sha256:catalog",
                "policyRevision": "sha256:policy"
            })
        );
    }

    #[test]
    fn project_asset_kinds_and_trust_have_explicit_wire_values() {
        assert_eq!(
            serde_json::to_value(ProjectAssetKind::Command).unwrap(),
            json!("command")
        );
        assert_eq!(
            serde_json::to_value(ProjectAssetKind::Permissions).unwrap(),
            json!("permissions")
        );
        assert_eq!(
            serde_json::to_value(ProjectAssetTrust::MixedReviewRequired).unwrap(),
            json!("mixedReviewRequired")
        );
    }

    #[test]
    fn outcome_estimate_covers_serialized_payload_and_spare_string_capacity() {
        let mut value = String::with_capacity(1_024 * 1_024);
        value.push('x');
        let outcome = StructuredOutcome {
            kind: OutcomeKind::Completed,
            side_effects_possible: false,
            result: Some(
                ToolResultEnvelope::new(Vec::new(), Some(Value::String(value)), false).unwrap(),
            ),
            error: None,
        };

        assert!(outcome.retained_bytes() >= serde_json::to_vec(&outcome).unwrap().len());
        assert!(outcome.retained_bytes() >= 1_024 * 1_024);
    }

    #[test]
    fn prepare_arguments_are_objects_with_a_bounded_encoding() {
        let request = PrepareRequest {
            cwd_handle: None,
            version: ContractVersion::V1,
            host: binding(),
            tool: ToolName::new("file_read").unwrap(),
            contract: ContractBinding {
                id: Identifier::new("file.read.v1").unwrap(),
                version: Identifier::new("v1").unwrap(),
                result_version: Identifier::new("v1").unwrap(),
            },
            arguments: json!({"filePath": "a"}),
        };
        assert!(
            request
                .validate(u64::try_from(MAX_ARGUMENT_BYTES).unwrap())
                .is_ok()
        );
        let mut scalar = request.clone();
        scalar.arguments = json!("a");
        assert!(
            scalar
                .validate(u64::try_from(MAX_ARGUMENT_BYTES).unwrap())
                .is_err()
        );
        let mut large = request;
        large.arguments = json!({"value": "x".repeat(32)});
        assert!(large.validate(8).is_err());

        let encoded = serde_json::to_value(&large).unwrap();
        assert!(serde_json::from_value::<PrepareRequest>(encoded).is_ok());
        assert!(
            serde_json::from_value::<PrepareRequest>(json!({
                "version":"v1",
                "host":binding(),
                "tool":"file_read",
                "contract":{"id":"file.read.v1","version":"v1","resultVersion":"v1"},
                "arguments":"not-an-object"
            }))
            .is_err()
        );
    }

    #[test]
    fn display_error_and_progress_text_have_independent_wire_bounds() {
        let display = format!("https://example.invalid/{}\npath", "x".repeat(2_048));
        assert_eq!(DisplayText::new(&display).unwrap().as_str(), display);
        assert!(ErrorText::new("first line\nsecond line").is_ok());

        let chunk = format!(
            "start\n\0\u{1b}{}",
            "x".repeat(MAX_PROGRESS_CHUNK_BYTES - 8)
        );
        assert_eq!(chunk.len(), MAX_PROGRESS_CHUNK_BYTES);
        let event = ProgressEvent {
            execution_id: Identifier::new("execution").unwrap(),
            sequence: 1,
            kind: Identifier::new("stdout").unwrap(),
            chunk: ProgressChunkText::new(&chunk).unwrap(),
        };
        assert_eq!(
            serde_json::from_value::<ProgressEvent>(serde_json::to_value(event).unwrap())
                .unwrap()
                .chunk
                .as_str(),
            chunk
        );
        assert!(ProgressChunkText::new(format!("{chunk}x")).is_err());
    }

    #[test]
    fn workspace_contracts_reject_absolute_paths_unknown_fields_and_oversized_batches() {
        assert!(WorkspacePath::new("/outside").is_err());
        assert!(WorkspacePath::new("windows\\path").is_err());
        assert!(WorkspacePath::new("sub/../file").is_ok());
        assert!(
            serde_json::from_value::<ListRequest>(json!({
                "version":"v1",
                "host":binding(),
                "cwdHandle":"cwd",
                "path":".",
                "recursive":true,
                "pageSize":10,
                "cursor":null,
                "tenant":"unsupported"
            }))
            .is_err()
        );
        let mutation = json!({"kind":"mkdir","path":"directory"});
        assert!(
            serde_json::from_value::<PrepareMutationRequest>(json!({
                "version":"v1",
                "host":binding(),
                "cwdHandle":"cwd",
                "mutations":vec![mutation; MAX_MUTATIONS + 1]
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<WatchPollRequest>(json!({
                "version":"v1",
                "host":binding(),
                "cwdHandle":"cwd",
                "subscriptionId":"subscription",
                "cursor":"cursor",
                "maxEvents":MAX_WATCH_POLL_EVENTS + 1,
                "maxBytes":MAX_WATCH_POLL_BYTES,
                "waitMs":0
            }))
            .unwrap()
            .validate()
            .is_err()
        );
    }

    #[test]
    fn watch_responses_require_ordered_events_or_an_explicit_full_resync() {
        let event = |sequence| WatchEvent {
            sequence,
            kind: WatchEventKind::Modify,
            path: WorkspacePath::new("file.txt").unwrap(),
        };
        let current = WatchPollResponse {
            version: ContractVersion::V1,
            subscription_id: Identifier::new("subscription").unwrap(),
            state: WatchState::Current,
            resync_reason: None,
            first_retained_sequence: Some(1),
            next_sequence: 3,
            events: vec![event(1), event(2)],
            next_cursor: Some(Cursor::new("cursor").unwrap()),
            expires_at_unix_ms: Some(1),
        };
        assert!(current.validate().is_ok());
        let mut unordered = current.clone();
        unordered.events.swap(0, 1);
        assert!(unordered.validate().is_err());
        let mut silent_gap = current;
        silent_gap.state = WatchState::FullResync;
        assert!(silent_gap.validate().is_err());

        let resync = WatchPollResponse {
            version: ContractVersion::V1,
            subscription_id: Identifier::new("subscription").unwrap(),
            state: WatchState::FullResync,
            resync_reason: Some(WatchResyncReason::RetentionLost),
            first_retained_sequence: None,
            next_sequence: 1,
            events: Vec::new(),
            next_cursor: None,
            expires_at_unix_ms: None,
        };
        assert!(resync.validate().is_ok());
    }

    #[test]
    fn nested_response_vectors_are_rejected_during_deserialization() {
        let resource = json!({
            "resourceId":"sha256:resource",
            "display":"a/path",
            "access":"read",
            "revision":null
        });
        assert!(
            serde_json::from_value::<PrepareResponse>(json!({
                "version":"v1",
                "preparationId":"preparation",
                "expiresAtUnixMs":1,
                "binding":{
                    "host":binding(),
                    "contract":{"id":"file.read.v1","version":"v1","resultVersion":"v1"},
                    "argumentDigest":"sha256:arguments"
                },
                "intent":{
                    "kind":"read",
                    "mutating":false,
                    "resources":vec![resource; MAX_RESOURCE_INTENTS + 1]
                }
            }))
            .is_err()
        );

        let progress = json!({
            "executionId":"execution",
            "sequence":1,
            "kind":"stdout",
            "chunk":"x"
        });
        assert!(
            serde_json::from_value::<StatusResponse>(json!({
                "version":"v1",
                "state":"running",
                "preparationId":"preparation",
                "invocationId":"invocation",
                "executionId":"execution",
                "expiresAtUnixMs":1,
                "binding":null,
                "outcome":null,
                "progressMetadata":{
                    "firstRetainedSequence":1,
                    "nextSequence":2,
                    "gapBeforeFirst":false
                },
                "progress":vec![progress; MAX_PROGRESS_EVENTS + 1]
            }))
            .is_err()
        );

        let content = json!({"type":"text","text":"bounded"});
        assert!(
            serde_json::from_value::<ToolResultEnvelope>(json!({
                "version":"v1",
                "content":vec![content; MAX_TOOL_RESULT_CONTENT + 1],
                "structuredContent":null,
                "isError":false
            }))
            .is_err()
        );
    }

    #[test]
    fn scm_mutation_paths_leave_room_for_the_repository_intent() {
        assert_eq!(MAX_SCM_PATHS + 1, MAX_RESOURCE_INTENTS);
        let path = serde_json::json!("file.txt");
        let accepted = serde_json::json!({
            "kind":"stage",
            "paths":vec![path.clone(); MAX_SCM_PATHS]
        });
        assert!(serde_json::from_value::<ScmMutation>(accepted).is_ok());
        let rejected = serde_json::json!({
            "kind":"stage",
            "paths":vec![path; MAX_SCM_PATHS + 1]
        });
        assert!(serde_json::from_value::<ScmMutation>(rejected).is_err());
    }

    #[test]
    fn a_commit_payload_without_a_body_key_is_absent_rather_than_empty() {
        let without_body = json!({
            "id":"sha256:commit",
            "parents":[],
            "authorName":"Workcell Test",
            "authorEmail":"workcell@example.invalid",
            "committedUnixSeconds":1,
            "summary":"subject line"
        });
        let absent = serde_json::from_value::<ScmCommit>(without_body.clone()).unwrap();
        assert_eq!(absent.body, None);

        let mut empty = absent.clone();
        empty.body = Some(ScmText::new("").unwrap());
        let mut populated = absent.clone();
        populated.body = Some(ScmText::new("first body line\nsecond body line").unwrap());
        assert_ne!(empty, absent);

        for value in [absent, empty, populated] {
            assert_eq!(
                serde_json::from_value::<ScmCommit>(serde_json::to_value(&value).unwrap()).unwrap(),
                value
            );
        }

        let mut null_body = without_body;
        null_body["body"] = json!(null);
        assert_eq!(
            serde_json::from_value::<ScmCommit>(null_body).unwrap().body,
            None
        );
    }

    fn binding() -> HostBinding {
        HostBinding {
            server_id: Identifier::new("server").unwrap(),
            instance_id: Identifier::new("instance").unwrap(),
            workspace_id: Identifier::new("workspace").unwrap(),
            workspace_generation: Identifier::new("generation").unwrap(),
            root_project_id: Identifier::new("project").unwrap(),
            principal_id: Identifier::new("principal").unwrap(),
            cwd_handle: ResourceId::new("sha256:cwd").unwrap(),
            catalog_revision: Revision::new("sha256:catalog").unwrap(),
            policy_revision: Revision::new("sha256:policy").unwrap(),
        }
    }
}
