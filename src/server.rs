use std::{
    borrow::Cow,
    collections::HashSet,
    fmt::{self, Write as _},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::FutureExt;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        CustomRequest, CustomResult, DiscoverResult, ErrorCode, ExtensionCapabilities,
        Implementation, InitializeRequestParams, InitializeResult, ListToolsResult,
        PaginatedRequestParams, ProgressNotificationParam, ProgressToken, ProtocolVersion,
        RequestMetaObject, ServerCapabilities, ServerInfo, Tool,
    },
    service::{Peer, RequestContext},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractBinding, ContractVersion, DIRECT_EXEC_CONTRACT_ID, DirectExecCapability, DisplayText,
    ErrorText, HostBinding, Identifier, MAX_ARGUMENT_BYTES, OperationBinding, OperationIntent,
    OperationKind, OutcomeKind, PROJECT_ASSET_MANIFEST_VERSION, ProjectAssetCapability,
    ProjectAssetLimits, ProjectAssetMethods, RemoteHostCapabilities, RemoteHostCwd,
    RemoteHostDescriptor, RemoteHostRevisions, RemoteHostToolCapability, RemoteHostToolLimits,
    RemoteOperationCapability, RemoteOperationMethods, ResourceAccess, ResourceId, ResourceIntent,
    Revision, SCM_DIFF_METHOD, SCM_DISCOVER_METHOD, SCM_LOG_METHOD, SCM_MUTATION_CONTRACT_ID,
    SCM_PREPARE_MUTATION_METHOD, SCM_READ_SIDE_METHOD, SCM_STATUS_METHOD,
    SNAPSHOT_ACKNOWLEDGE_METHOD, SNAPSHOT_CAPTURE_METHOD, SNAPSHOT_CLEANUP_CONTRACT_ID,
    SNAPSHOT_INSPECT_METHOD, SNAPSHOT_PREPARE_CLEANUP_METHOD, SNAPSHOT_PREPARE_RESTORE_METHOD,
    SNAPSHOT_PREPARE_UNREVERT_METHOD, SNAPSHOT_RESTORE_CONTRACT_ID, SNAPSHOT_STATUS_METHOD,
    SNAPSHOT_UNREVERT_CONTRACT_ID, ScmCapability, ScmDiffRequest, ScmDiscoverRequest, ScmLimits,
    ScmLogRequest, ScmMethods, ScmMutation, ScmPrepareMutationRequest, ScmPrepareMutationResponse,
    ScmReadSideRequest, ScmStatusRequest, SnapshotAcknowledgeRequest, SnapshotCaptureRequest,
    SnapshotInspectRequest, SnapshotPrepareCleanupRequest, SnapshotPrepareCleanupResponse,
    SnapshotPrepareRestoreRequest, SnapshotPrepareRestoreResponse, SnapshotPrepareUnrevertRequest,
    SnapshotRestorePreview, SnapshotStatusRequest, StructuredOutcome, ToolResultContent,
    ToolResultEnvelope, ToolResultText, WORKSPACE_MUTATION_CONTRACT_ID, WorkspaceCapability,
    WorkspaceLimits, WorkspaceMethods, WorkspaceMutationCapability, WorkspaceRequestBinding,
    WorkspaceWatchCapability, WorkspaceWatchLimits, WorkspaceWatchMethods,
};
use workcell_mcp_code::{CodeBuildError, CodeConfiguration, CodeInput, CodeToolGroup};
use workcell_mcp_code_graph::{
    CodeContextInput, CodeExpandInput, CodeGraphLimits, CodeGraphToolGroup, CodeImpactInput,
    CodeMapInput, CodeRefsInput, GraphProgress, GraphProgressSink, ModelText as GraphModelText,
    SelectorRefusal, Shrinkable,
};
use workcell_mcp_files::{
    FileApplyPatchInput, FileEditInput, FileGlobInput, FileGrepInput, FileReadInput, FileResource,
    FileResourceAccess, FileToolGroup, FileWriteInput, FilesystemLimits, IndexInput,
    ModelText as FileModelText, RootResourceKind, WorkspaceError, root_relative_resource_id,
};
use workcell_mcp_shell::{
    ShellInput, ShellPermissionPolicy, ShellProgressChunk, ShellProgressSink, ShellToolGroup,
    mcp_progress_sink,
};
use workcell_mcp_web::{
    PreparedWebOperation, ProxyConfiguration, WebOperationExecution, WebToolGroup, WebfetchInput,
    WebsearchExecutionConfiguration, WebsearchInput,
};
use workcell_tool_contract::{CatalogRevision, ToolManifest, ToolSpec};
use workcell_workspace_scm::{ScmError, ScmGroup};
use workcell_workspace_snapshot::{SnapshotError, SnapshotManager};

#[cfg(unix)]
#[path = "transfer/host.rs"]
mod reviewed_host;

use crate::{
    cli::ToolGroup,
    execution_environment::{
        ExecutionEnvironmentDisclosure, TOOL_NAME as EXECUTION_ENVIRONMENT_TOOL,
        ToolGroupDisclosure, spec as execution_environment_spec,
        tool as execution_environment_tool,
    },
    remote_host::{
        BeginExecution, CANCEL_METHOD, CancelRequest, DISCOVER_PROJECT_ASSETS_METHOD,
        DiscoverProjectAssetsRequest, EXECUTE_METHOD, EXTENSION_ID,
        LARGE_PREPARATION_RESERVATION_BYTES, LIST_METHOD, ListRequest,
        MAX_PREPARED_OPERATION_BYTES, MEDIUM_PREPARATION_RESERVATION_BYTES, PREPARE_EXEC_METHOD,
        PREPARE_METHOD, PREPARE_MUTATION_METHOD, PrepareExecRequest, PrepareMutationRequest,
        PrepareRequest, PreparedRemoteOperation, READ_PROJECT_ASSET_METHOD, READ_TEXT_METHOD,
        RELEASE_METHOD, RESOLVE_DIRECTORY_METHOD, ReadProjectAssetRequest, ReadTextRequest,
        ReleaseRequest, RemoteHostConfiguration, RemoteHostState, ResolveDirectoryRequest,
        SEARCH_TEXT_METHOD, SMALL_PREPARATION_RESERVATION_BYTES, STAT_METHOD, STATUS_METHOD,
        SearchTextRequest, StatRequest, StatusRequest, WATCH_CLOSE_METHOD, WATCH_OPEN_METHOD,
        WATCH_POLL_METHOD, WatchCloseRequest, WatchOpenRequest, WatchPollRequest,
    },
    transfer::TransferGroup,
};

const MODERN_PROTOCOLS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];
const DUAL_ERA_PROTOCOLS: &[ProtocolVersion] =
    &[ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25];
const RESOURCE_NAMESPACE_VERSION: &str = "v1";
const ISOLATED_PYTHON_RESOURCE: &str = "isolated-python";
static PROCESS_INSTANCE_ID: OnceLock<Arc<str>> = OnceLock::new();

pub(crate) fn protocol_versions(modern_only: bool) -> &'static [ProtocolVersion] {
    if modern_only {
        MODERN_PROTOCOLS
    } else {
        DUAL_ERA_PROTOCOLS
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServerBehavior {
    pub expose_execution_environment: bool,
    pub modern_only: bool,
}

/// Per-group startup configuration, all of it operator-chosen and unreachable from tool input.
///
/// Grouped into one value because each field belongs to exactly one tool group; passing them
/// positionally made the relationship between a flag and its group easy to get wrong.
pub struct ToolConfiguration<'a> {
    pub allow_write: bool,
    pub web: WebsearchExecutionConfiguration,
    pub web_icons: bool,
    pub proxy: ProxyConfiguration,
    pub shell_policy: ShellPermissionPolicy,
    pub shell_output_filter: bool,
    /// Applies to every group that traverses: the file tools and the code graph.
    pub honor_gitignore: bool,
    pub code: CodeConfiguration<'a>,
    pub max_transfer_bytes: usize,
    pub snapshot_root: Option<&'a Path>,
    pub transfer_root: Option<&'a Path>,
    pub snapshot_exclusions: &'a [PathBuf],
}

#[derive(Clone)]
pub struct WorkcellServer {
    files: Option<FileToolGroup>,
    workspace_files: Option<FileToolGroup>,
    scm: Option<ScmGroup>,
    // The code-graph group owns a bounded extraction cache behind a mutex, which is shared rather
    // than cloned so every server clone reuses the same warm facts.
    code_graph: Option<Arc<CodeGraphToolGroup>>,
    web: Option<WebToolGroup>,
    shell: Option<ShellToolGroup>,
    // The code group owns a worker pool, which is shared rather than cloned so every server clone
    // draws on the same bounded set of subprocesses.
    code: Option<Arc<CodeToolGroup>>,
    transfer: Option<TransferGroup>,
    execution_environment: Option<ExecutionEnvironmentDisclosure>,
    catalog: Arc<[Tool]>,
    manifest: Arc<ToolManifest>,
    catalog_revision: CatalogRevision,
    policy_revision: CatalogRevision,
    root: Option<Arc<PathBuf>>,
    instance_id: Arc<str>,
    remote_host: Option<RemoteHostState>,
    snapshots: Option<SnapshotManager>,
    snapshot_root: Option<Arc<PathBuf>>,
    transfer_root: Option<Arc<PathBuf>>,
    snapshot_exclusions: Arc<[PathBuf]>,
    modern_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerBuildError {
    Filesystem,
    CodeWorker(CodeBuildError),
    DuplicateToolName,
    CatalogSerialization,
    IncompleteExactPreparation,
    SnapshotStorage,
    TransferStorage,
}

impl fmt::Display for ServerBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filesystem => {
                formatter.write_str("filesystem or shell tools could not be initialized")
            }
            // The inner message names the missing configuration, which is what an operator needs.
            Self::CodeWorker(error) => write!(formatter, "{error}"),
            Self::DuplicateToolName => {
                formatter.write_str("tool catalog contains a duplicate name")
            }
            Self::CatalogSerialization => {
                formatter.write_str("tool catalog could not be serialized")
            }
            Self::IncompleteExactPreparation => {
                formatter.write_str("configured catalog contains a tool without exact preparation")
            }
            Self::SnapshotStorage => {
                formatter.write_str("workspace snapshot storage is invalid or unhealthy")
            }
            Self::TransferStorage => {
                formatter.write_str("reviewed transfer storage could not be initialized")
            }
        }
    }
}

impl std::error::Error for ServerBuildError {}

impl WorkcellServer {
    pub async fn configured(
        root: Option<&Path>,
        groups: &[ToolGroup],
        behavior: ServerBehavior,
        tools: ToolConfiguration<'_>,
    ) -> Result<Self, ServerBuildError> {
        let shell_policy_revision = tools
            .shell_policy
            .revision()
            .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let mut policy_groups = groups
            .iter()
            .map(|group| group.as_str())
            .collect::<Vec<_>>();
        policy_groups.sort_unstable();
        let policy_revision = CatalogRevision::for_serializable(&serde_json::json!({
            "version": "v1",
            "groups": policy_groups,
            "allowWrite": tools.allow_write,
            "webIcons": groups.contains(&ToolGroup::Web).then_some(tools.web_icons),
            "shell": groups.contains(&ToolGroup::Shell).then_some(shell_policy_revision),
            "shellOutputFilter": groups.contains(&ToolGroup::Shell).then_some(tools.shell_output_filter),
            "codeTypeCheck": groups.contains(&ToolGroup::PythonExecution).then_some(tools.code.type_check),
            "maxTransferBytes": groups.contains(&ToolGroup::Transfer).then_some(tools.max_transfer_bytes),
            "snapshots": tools.snapshot_root.is_some(),
            "reviewedTransfer": tools.transfer_root.is_some(),
        }))
        .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let filesystem_limits = FilesystemLimits {
            honor_gitignore: tools.honor_gitignore,
            ..FilesystemLimits::default()
        };
        let files = if groups.contains(&ToolGroup::Files) {
            Some(
                FileToolGroup::new(
                    root.ok_or(ServerBuildError::Filesystem)?,
                    tools.allow_write,
                    Some(filesystem_limits),
                )
                .await
                .map_err(|_| ServerBuildError::Filesystem)?,
            )
        } else {
            None
        };
        // Constructed over its own read-only `FileToolGroup` so the group stands alone when the
        // files tools are not exposed. Confinement is identical; only reads are ever performed.
        let code_graph = if groups.contains(&ToolGroup::CodeGraph) {
            let limits = CodeGraphLimits {
                honor_gitignore: tools.honor_gitignore,
                ..CodeGraphLimits::default()
            };
            Some(Arc::new(
                CodeGraphToolGroup::new(root.ok_or(ServerBuildError::Filesystem)?, Some(limits))
                    .await
                    .map_err(|_| ServerBuildError::Filesystem)?,
            ))
        } else {
            None
        };
        let current_year = current_utc_year();
        let web_specs = if groups.contains(&ToolGroup::Web) {
            workcell_mcp_web::specs(current_year, &tools.web)
        } else {
            Vec::new()
        };
        let web = groups
            .contains(&ToolGroup::Web)
            .then(|| WebToolGroup::production_with_proxy(tools.web, tools.web_icons, &tools.proxy));
        let shell = if groups.contains(&ToolGroup::Shell) {
            Some(
                ShellToolGroup::with_policy(
                    root.ok_or(ServerBuildError::Filesystem)?,
                    tools.shell_policy,
                )
                .await
                .map_err(|_| ServerBuildError::Filesystem)?
                .with_output_filter(tools.shell_output_filter),
            )
        } else {
            None
        };
        // Building the group starts a worker, so a missing or unrunnable binary fails here.
        let code = if groups.contains(&ToolGroup::PythonExecution) {
            Some(Arc::new(
                CodeToolGroup::new(tools.code)
                    .await
                    .map_err(ServerBuildError::CodeWorker)?,
            ))
        } else {
            None
        };
        let mut transfer = if groups.contains(&ToolGroup::Transfer) {
            Some(
                TransferGroup::new(
                    root.ok_or(ServerBuildError::Filesystem)?,
                    tools.allow_write,
                    tools.max_transfer_bytes,
                )
                .await
                .map_err(|_| ServerBuildError::Filesystem)?,
            )
        } else {
            None
        };
        if let (Some(transfer), Some(files)) = (&mut transfer, &files) {
            transfer.share_files(files.clone());
        }
        let manifest = ToolManifest::new(&compose_specs([
            if groups.contains(&ToolGroup::Files) {
                workcell_mcp_files::specs(tools.allow_write)
            } else {
                Vec::new()
            },
            groups
                .contains(&ToolGroup::CodeGraph)
                .then(workcell_mcp_code_graph::specs)
                .unwrap_or_default(),
            web_specs,
            groups
                .contains(&ToolGroup::Shell)
                .then(workcell_mcp_shell::specs)
                .unwrap_or_default(),
            groups
                .contains(&ToolGroup::PythonExecution)
                .then(workcell_mcp_code::specs)
                .unwrap_or_default(),
            if behavior.expose_execution_environment {
                vec![execution_environment_spec()]
            } else {
                Vec::new()
            },
        ])?)
        .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let catalog_revision = manifest.revision.clone();
        let catalog = compose_catalog([
            files.as_ref().map_or_else(Vec::new, FileToolGroup::catalog),
            code_graph
                .as_ref()
                .map_or_else(Vec::new, |_| workcell_mcp_code_graph::catalog()),
            web.as_ref()
                .map_or_else(Vec::new, |group| group.catalog(current_year)),
            shell
                .as_ref()
                .map_or_else(Vec::new, ShellToolGroup::catalog),
            code.as_ref().map_or_else(Vec::new, |group| group.catalog()),
            if behavior.expose_execution_environment {
                vec![execution_environment_tool()]
            } else {
                Vec::new()
            },
        ])?;
        if catalog
            .iter()
            .map(|tool| tool.name.as_ref())
            .ne(manifest.tools.iter().map(|tool| tool.name.as_str()))
        {
            return Err(ServerBuildError::CatalogSerialization);
        }
        let execution_environment = if behavior.expose_execution_environment {
            Some(ExecutionEnvironmentDisclosure::collect(root).await)
        } else {
            None
        };
        Ok(Self {
            files,
            workspace_files: None,
            scm: None,
            code_graph,
            web,
            shell,
            code,
            transfer,
            execution_environment,
            catalog: catalog.into(),
            manifest: Arc::new(manifest),
            catalog_revision,
            policy_revision,
            root: root.map(|path| Arc::new(path.to_path_buf())),
            instance_id: process_instance_id(),
            remote_host: None,
            snapshots: None,
            snapshot_root: tools.snapshot_root.map(|path| Arc::new(path.to_path_buf())),
            transfer_root: tools.transfer_root.map(|path| Arc::new(path.to_path_buf())),
            snapshot_exclusions: tools.snapshot_exclusions.to_vec().into(),
            modern_only: behavior.modern_only,
        })
    }

    /// Releases pooled worker processes during graceful shutdown.
    pub async fn shutdown(&self) {
        if let Some(code) = &self.code {
            code.shutdown().await;
        }
    }

    #[must_use]
    pub fn catalog(&self) -> Vec<Tool> {
        self.catalog.to_vec()
    }

    #[must_use]
    pub const fn catalog_revision(&self) -> &CatalogRevision {
        &self.catalog_revision
    }

    #[must_use]
    pub fn tool_manifest(&self) -> &ToolManifest {
        &self.manifest
    }

    pub(crate) async fn with_remote_host(
        self,
        configuration: RemoteHostConfiguration,
    ) -> Result<Self, ServerBuildError> {
        self.with_remote_host_git(configuration, PathBuf::from("git"))
            .await
    }

    async fn with_remote_host_git(
        mut self,
        configuration: RemoteHostConfiguration,
        git_executable: PathBuf,
    ) -> Result<Self, ServerBuildError> {
        if !self
            .catalog
            .iter()
            .all(|tool| PreparedRemoteOperation::supports(tool.name.as_ref()))
        {
            return Err(ServerBuildError::IncompleteExactPreparation);
        }
        let root = self.root.as_ref().ok_or(ServerBuildError::Filesystem)?;
        let workspace_files = if let Some(files) = &self.files {
            files.clone()
        } else if let Some(transfer) = &self.transfer {
            transfer.files().clone()
        } else {
            FileToolGroup::new(root.as_ref(), false, None)
                .await
                .map_err(|_| ServerBuildError::Filesystem)?
        };
        let cwd = workspace_files
            .workspace_root()
            .await
            .map_err(|_| ServerBuildError::Filesystem)?;
        let scm = ScmGroup::probe(workspace_files.clone(), git_executable).await;
        let workspace_binding = durable_workspace_binding(&configuration)
            .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let snapshots = match &self.snapshot_root {
            Some(snapshot_root) => Some(
                SnapshotManager::open_bound(
                    workspace_files.workspace_snapshot_access(),
                    snapshot_root.as_ref(),
                    &self.snapshot_exclusions,
                    &workspace_binding,
                )
                .await
                .map_err(|_| ServerBuildError::SnapshotStorage)?,
            ),
            None => None,
        };
        let mut control_plane_missing = Vec::new();
        if !workspace_files.allow_write() {
            control_plane_missing.push("workspaceMutation");
        }
        if self.shell.is_none() {
            control_plane_missing.push("directExec");
        }
        if snapshots.is_none() {
            control_plane_missing.push("snapshots");
        }
        if scm.is_none() {
            control_plane_missing.push("scm");
        }
        let control_plane = control_plane_missing.is_empty();
        let control_plane_missing = control_plane_missing
            .into_iter()
            .map(Identifier::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let environment_revision = if let Some(environment) = &self.execution_environment {
            environment
                .startup(self.tool_group_disclosure())
                .ok_or(ServerBuildError::CatalogSerialization)?
                .snapshot_revision
        } else {
            CatalogRevision::for_serializable(&serde_json::json!({
                "version": "v1",
                "disclosed": false,
            }))
            .map_err(|_| ServerBuildError::CatalogSerialization)?
            .as_str()
            .to_owned()
        };
        let tool_limits = RemoteHostToolLimits {
            max_request_bytes: u64::try_from(crate::http_policy::MAX_JSON_BODY_BYTES)
                .map_err(|_| ServerBuildError::CatalogSerialization)?,
        };
        let tool_capability = RemoteHostToolCapability {
            version: ContractVersion::V1,
            limits: tool_limits.clone(),
        };
        let instance_id = Identifier::new(self.instance_id.to_string())
            .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let cwd_handle = cwd.handle.clone();
        let catalog_revision = Revision::new(self.catalog_revision.as_str())
            .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let policy_revision = Revision::new(self.policy_revision.as_str())
            .map_err(|_| ServerBuildError::CatalogSerialization)?;
        let binding = HostBinding {
            server_id: configuration.server_id.clone(),
            instance_id: instance_id.clone(),
            workspace_id: configuration.workspace_id.clone(),
            workspace_generation: configuration.workspace_generation.clone(),
            root_project_id: configuration.root_project_id.clone(),
            principal_id: configuration.principal_id.clone(),
            cwd_handle: cwd_handle.clone(),
            catalog_revision: catalog_revision.clone(),
            policy_revision: policy_revision.clone(),
        };
        #[cfg(unix)]
        if let Some(root) = &self.transfer_root {
            let transfer = self
                .transfer
                .as_mut()
                .ok_or(ServerBuildError::TransferStorage)?;
            transfer.reviewed = Some(
                crate::transfer::reviewed::ReviewedTransfers::open(
                    workspace_files.clone(),
                    binding.clone(),
                    transfer.max_transfer_bytes() as u64,
                    root,
                    &workspace_binding,
                )
                .map_err(|_| ServerBuildError::TransferStorage)?,
            );
        }
        #[cfg(not(unix))]
        if self.transfer_root.is_some() {
            return Err(ServerBuildError::TransferStorage);
        }
        let descriptor = RemoteHostDescriptor {
            version: ContractVersion::V1,
            server_id: configuration.server_id,
            workspace_id: configuration.workspace_id,
            workspace_generation: configuration.workspace_generation,
            root_project_id: configuration.root_project_id,
            principal_id: configuration.principal_id,
            instance_id,
            resource_namespace_version: Identifier::new(RESOURCE_NAMESPACE_VERSION)
                .map_err(|_| ServerBuildError::CatalogSerialization)?,
            path_style: Identifier::new("root-relative-posix")
                .map_err(|_| ServerBuildError::CatalogSerialization)?,
            revisions: RemoteHostRevisions {
                execution_environment: Revision::new(environment_revision)
                    .map_err(|_| ServerBuildError::CatalogSerialization)?,
                catalog: catalog_revision,
                policy: policy_revision,
            },
            cwd: RemoteHostCwd {
                handle: cwd_handle,
                display_path: DisplayText::new(cwd.display_path.as_str())
                    .map_err(|_| ServerBuildError::CatalogSerialization)?,
            },
            capabilities: RemoteHostCapabilities {
                tool_catalog: tool_capability.clone(),
                tool_execution: tool_capability,
                execution_environment: self.execution_environment.as_ref().map(|_| {
                    RemoteHostToolCapability {
                        version: ContractVersion::V1,
                        limits: tool_limits,
                    }
                }),
                reviewed_transfer: {
                    #[cfg(unix)]
                    {
                        self.transfer
                            .as_ref()
                            .and_then(|transfer| transfer.reviewed.as_ref())
                            .map(|manager| manager.capability())
                    }
                    #[cfg(not(unix))]
                    {
                        None
                    }
                },
                operations: Some(RemoteOperationCapability {
                    version: ContractVersion::V1,
                    exact_preparation: true,
                    methods: RemoteOperationMethods {
                        prepare: true,
                        execute: true,
                        release: true,
                        status: true,
                        cancel: true,
                    },
                    limits: RemoteHostState::limits(),
                }),
                workspace: Some(WorkspaceCapability {
                    version: ContractVersion::V1,
                    methods: WorkspaceMethods {
                        resolve_directory: true,
                        stat: true,
                        list: true,
                        read_text: true,
                        search_text: true,
                    },
                    limits: WorkspaceLimits {
                        max_path_bytes: u32::try_from(
                            workcell_host_contract::MAX_WORKSPACE_PATH_BYTES,
                        )
                        .unwrap_or(u32::MAX),
                        max_page_size: workcell_host_contract::MAX_PAGE_SIZE,
                        max_text_read_bytes: workcell_host_contract::MAX_TEXT_READ_BYTES,
                        max_search_pattern_bytes: u32::try_from(
                            workcell_host_contract::MAX_SEARCH_PATTERN_BYTES,
                        )
                        .unwrap_or(u32::MAX),
                        max_cursor_bytes: u32::try_from(workcell_host_contract::MAX_CURSOR_BYTES)
                            .unwrap_or(u32::MAX),
                        max_list_entries: workcell_host_contract::MAX_WORKSPACE_LIST_ENTRIES,
                        max_list_retained_bytes:
                            workcell_host_contract::MAX_WORKSPACE_LIST_RETAINED_BYTES,
                        max_list_hash_bytes: workcell_host_contract::MAX_WORKSPACE_LIST_HASH_BYTES,
                    },
                }),
                watch: Some(WorkspaceWatchCapability {
                    version: ContractVersion::V1,
                    methods: WorkspaceWatchMethods {
                        open: true,
                        poll: true,
                        close: true,
                    },
                    limits: WorkspaceWatchLimits {
                        max_subscriptions: u32::try_from(
                            workcell_host_contract::MAX_WATCH_SUBSCRIPTIONS,
                        )
                        .unwrap_or(u32::MAX),
                        max_retained_events: u32::try_from(
                            workcell_host_contract::MAX_WATCH_RETAINED_EVENTS,
                        )
                        .unwrap_or(u32::MAX),
                        max_retained_bytes: u64::try_from(
                            workcell_host_contract::MAX_WATCH_RETAINED_BYTES,
                        )
                        .unwrap_or(u64::MAX),
                        max_lifetime_events: u64::try_from(
                            workcell_host_contract::MAX_WATCH_LIFETIME_EVENTS,
                        )
                        .unwrap_or(u64::MAX),
                        max_poll_events: workcell_host_contract::MAX_WATCH_POLL_EVENTS,
                        max_poll_bytes: workcell_host_contract::MAX_WATCH_POLL_BYTES,
                        max_wait_ms: workcell_host_contract::MAX_WATCH_WAIT_MS,
                        subscription_ttl_ms: workcell_host_contract::WATCH_SUBSCRIPTION_TTL_MS,
                    },
                    recursive: true,
                    exact_rename_pairing: false,
                }),
                project_assets: Some(ProjectAssetCapability {
                    version: ContractVersion::V1,
                    manifest_version: Identifier::new(PROJECT_ASSET_MANIFEST_VERSION)
                        .map_err(|_| ServerBuildError::CatalogSerialization)?,
                    methods: ProjectAssetMethods {
                        discover: true,
                        read: true,
                    },
                    limits: ProjectAssetLimits {
                        max_assets: u32::try_from(workcell_host_contract::MAX_PROJECT_ASSETS)
                            .unwrap_or(u32::MAX),
                        max_read_bytes: workcell_host_contract::MAX_PROJECT_ASSET_READ_BYTES,
                        max_path_bytes: u32::try_from(
                            workcell_host_contract::MAX_WORKSPACE_PATH_BYTES,
                        )
                        .unwrap_or(u32::MAX),
                        max_discovery_entries: workcell_host_contract::MAX_WORKSPACE_LIST_ENTRIES,
                        max_discovery_retained_bytes:
                            workcell_host_contract::MAX_WORKSPACE_LIST_RETAINED_BYTES,
                        max_discovery_hash_bytes:
                            workcell_host_contract::MAX_WORKSPACE_LIST_HASH_BYTES,
                    },
                }),
                workspace_mutation: workspace_files.allow_write().then_some(
                    WorkspaceMutationCapability {
                        version: ContractVersion::V1,
                        prepared: true,
                        max_mutations: u32::try_from(workcell_host_contract::MAX_MUTATIONS)
                            .unwrap_or(u32::MAX),
                        max_content_bytes: u64::try_from(
                            workcell_host_contract::MAX_MUTATION_CONTENT_BYTES,
                        )
                        .unwrap_or(u64::MAX),
                        atomic_across_files: false,
                        rollback_on_failure: true,
                    },
                ),
                direct_exec: self.shell.as_ref().map(|_| DirectExecCapability {
                    version: ContractVersion::V1,
                    prepared: true,
                    interactive: false,
                    max_command_bytes: u32::try_from(workcell_host_contract::MAX_COMMAND_BYTES)
                        .unwrap_or(u32::MAX),
                    max_timeout_ms: workcell_mcp_shell::MAX_TIMEOUT_MS,
                }),
                scm: scm.as_ref().map(|_| ScmCapability {
                    version: ContractVersion::V1,
                    methods: ScmMethods {
                        discover: true,
                        status: true,
                        log: true,
                        diff: true,
                        read_side: true,
                        stage: workspace_files.allow_write(),
                        unstage: workspace_files.allow_write(),
                        discard: workspace_files.allow_write(),
                    },
                    limits: ScmLimits {
                        max_concurrent_operations: u32::try_from(
                            workcell_workspace_scm::MAX_CONCURRENT_SCM_OPERATIONS,
                        )
                        .unwrap_or(u32::MAX),
                        max_paths: u32::try_from(workcell_host_contract::MAX_SCM_PATHS)
                            .unwrap_or(u32::MAX),
                        max_status_entries: workcell_host_contract::MAX_SCM_STATUS_ENTRIES,
                        max_status_paths: workcell_host_contract::MAX_SCM_STATUS_PATHS,
                        max_config_bytes: workcell_host_contract::MAX_SCM_CONFIG_BYTES,
                        max_log_entries: workcell_host_contract::MAX_SCM_LOG_ENTRIES,
                        max_log_commits: workcell_host_contract::MAX_SCM_LOG_COMMITS,
                        max_commit_bytes: workcell_host_contract::MAX_SCM_COMMIT_BYTES,
                        max_log_scan_bytes: workcell_host_contract::MAX_SCM_LOG_SCAN_BYTES,
                        max_diff_lines: workcell_host_contract::MAX_SCM_DIFF_LINES,
                        max_diff_bytes: workcell_host_contract::MAX_SCM_DIFF_BYTES,
                        max_diff_files: workcell_host_contract::MAX_SCM_DIFF_FILES,
                        max_diff_scan_bytes: workcell_host_contract::MAX_SCM_DIFF_SCAN_BYTES,
                        max_diff_parsed_lines: workcell_host_contract::MAX_SCM_DIFF_PARSED_LINES,
                        max_side_lines: workcell_host_contract::MAX_SCM_SIDE_LINES,
                        max_side_bytes: workcell_host_contract::MAX_SCM_SIDE_BYTES,
                        max_cursor_bytes: u32::try_from(workcell_host_contract::MAX_CURSOR_BYTES)
                            .unwrap_or(u32::MAX),
                    },
                    prepared_mutations: workspace_files.allow_write(),
                    discard_untracked: false,
                }),
                snapshots: snapshots.as_ref().map(|_| SnapshotManager::capability()),
                control_plane,
                control_plane_missing,
            },
        };
        self.workspace_files = Some(workspace_files);
        self.scm = scm;
        self.snapshots = snapshots;
        self.remote_host = Some(RemoteHostState::new(descriptor, binding));
        Ok(self)
    }

    #[must_use]
    pub const fn modern_only(&self) -> bool {
        self.modern_only
    }

    /// The byte route is available only after authenticated remote-host setup opens private storage.
    #[must_use]
    pub(crate) fn transfer(&self) -> Option<&TransferGroup> {
        self.transfer.as_ref().filter(|transfer| transfer.enabled())
    }

    fn validate_request_context(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<ProtocolVersion, ErrorData> {
        let requested = context.protocol_version().ok_or_else(|| {
            ErrorData::invalid_params("request protocol version is required", None)
        })?;
        let supported = self.supported_protocol_versions();
        if !supported.contains(&requested) {
            return Err(ErrorData::unsupported_protocol_version(
                requested, &supported,
            ));
        }
        if requested == ProtocolVersion::V_2026_07_28 {
            let missing = context
                .meta
                .missing_required_keys(&ProtocolVersion::V_2026_07_28);
            if !missing.is_empty() {
                return Err(ErrorData::invalid_params(
                    format!(
                        "request _meta is missing or has malformed required fields: {}",
                        missing.join(", ")
                    ),
                    None,
                ));
            }
        }
        Ok(requested)
    }

    fn validate_discover_context(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let requested = context.meta.protocol_version().ok_or_else(|| {
            ErrorData::invalid_params("request protocol version is required", None)
        })?;
        if requested != ProtocolVersion::V_2026_07_28 {
            return Err(ErrorData::unsupported_protocol_version(
                requested,
                MODERN_PROTOCOLS,
            ));
        }
        let missing = context
            .meta
            .missing_required_keys(&ProtocolVersion::V_2026_07_28);
        if !missing.is_empty() {
            return Err(ErrorData::invalid_params(
                format!(
                    "request _meta is missing or has malformed required fields: {}",
                    missing.join(", ")
                ),
                None,
            ));
        }
        Ok(())
    }

    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Result<CallToolResult, ErrorData> {
        self.dispatch_with_context(name, arguments, cancellation, None)
            .await
    }

    async fn dispatch_with_context(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
        progress: Option<ToolProgressContext>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(files) = &self.files
            && let Some(result) = files
                .dispatch(name, arguments.clone(), cancellation.clone())
                .await
        {
            return result;
        }
        if let Some(code_graph) = &self.code_graph
            && let Some(result) = code_graph
                .dispatch(name, arguments.clone(), cancellation.clone())
                .await
        {
            return result;
        }
        if let Some(web) = &self.web
            && let Some(result) = web
                .dispatch(name, arguments.clone(), cancellation.clone())
                .await
        {
            return result;
        }
        if let Some(code) = &self.code
            && let Some(result) = code
                .dispatch(name, arguments.clone(), cancellation.clone())
                .await
        {
            return result;
        }
        if name == EXECUTION_ENVIRONMENT_TOOL
            && let Some(execution_environment) = &self.execution_environment
        {
            return Ok(execution_environment
                .call_tool(arguments, self.tool_group_disclosure(), cancellation)
                .await);
        }
        if let Some(shell) = &self.shell
            && let Some(result) = shell
                .dispatch_with_progress(
                    name,
                    arguments,
                    cancellation,
                    progress.map(ToolProgressContext::into_sink),
                )
                .await
        {
            return result;
        }
        Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            "Unknown tool",
            None,
        ))
    }

    async fn prepare_remote(
        &self,
        remote: &RemoteHostState,
        request: PrepareRequest,
        token: &CancellationToken,
    ) -> Result<workcell_host_contract::PrepareResponse, ErrorData> {
        remote.validate_host(&request.host).map_err(remote_error)?;
        request
            .validate(RemoteHostState::limits().max_argument_bytes)
            .map_err(|_| remote_error(crate::remote_host::RemoteOperationError::InvalidRequest))?;
        let reservation = remote
            .reserve_preparation_wait(preparation_reservation_bytes(request.tool.as_str()), token)
            .await
            .map_err(remote_error)?;
        let tool = self
            .catalog
            .iter()
            .find(|tool| tool.name.as_ref() == request.tool.as_str())
            .ok_or_else(|| ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Unknown tool", None))?;
        let contract = contract_binding(tool)?;
        if contract != request.contract {
            return Err(remote_error(
                crate::remote_host::RemoteOperationError::ContractMismatch,
            ));
        }
        let arguments = if let Some(cwd) = &request.cwd_handle {
            let path = self
                .workspace_files
                .as_ref()
                .ok_or_else(remote_invalid)?
                .workspace_directory_path(cwd)
                .await
                .map_err(workspace_error)?;
            cwd_tool_arguments(request.tool.as_str(), request.arguments, &path)?
        } else {
            request.arguments
        };
        let argument_digest = argument_digest(&arguments)?;
        let (operation, intent) = self
            .prepare_operation(request.tool.as_str(), arguments)
            .await?;
        let binding = OperationBinding {
            host: request.host,
            contract,
            argument_digest,
        };
        remote
            .prepare_reserved(reservation, operation, binding, intent)
            .map_err(remote_error)
    }

    async fn prepare_workspace_mutation(
        &self,
        remote: &RemoteHostState,
        request: PrepareMutationRequest,
        token: &CancellationToken,
    ) -> Result<workcell_host_contract::PrepareResponse, ErrorData> {
        remote
            .validate_host(&request.binding.host)
            .map_err(remote_error)?;
        request.validate().map_err(|_| remote_invalid())?;
        let encoded = serde_json::to_value(&request).map_err(|_| remote_invalid())?;
        let reservation = remote
            .reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, token)
            .await
            .map_err(remote_error)?;
        let prepared = self
            .workspace_files
            .as_ref()
            .ok_or_else(method_not_found)?
            .prepare_workspace_mutation_bounded(
                &request.binding.cwd_handle,
                request.mutations,
                MAX_PREPARED_OPERATION_BYTES,
                &CancellationToken::new(),
            )
            .await
            .map_err(workspace_error)?;
        let mut resources = file_intents(prepared.resources())?;
        for (resource, revision) in resources.iter_mut().zip(prepared.resource_revisions()) {
            resource.revision.clone_from(revision);
        }
        let intent = OperationIntent {
            kind: OperationKind::Mutate,
            mutating: true,
            resources,
        };
        let binding = OperationBinding {
            host: request.binding.host,
            contract: fixed_contract(WORKSPACE_MUTATION_CONTRACT_ID)?,
            argument_digest: argument_digest(&encoded)?,
        };
        remote
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::WorkspaceMutation(prepared),
                binding,
                intent,
            )
            .map_err(remote_error)
    }

    async fn prepare_direct_exec(
        &self,
        remote: &RemoteHostState,
        request: PrepareExecRequest,
        token: &CancellationToken,
    ) -> Result<workcell_host_contract::PrepareResponse, ErrorData> {
        remote
            .validate_host(&request.binding.host)
            .map_err(remote_error)?;
        let encoded = serde_json::to_value(&request).map_err(|_| remote_invalid())?;
        let reservation = remote
            .reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, token)
            .await
            .map_err(remote_error)?;
        let relative_workdir = self
            .workspace_files
            .as_ref()
            .ok_or_else(method_not_found)?
            .workspace_directory_path(&request.binding.cwd_handle)
            .await
            .map_err(workspace_error)?;
        let shell = self.shell.as_ref().ok_or_else(method_not_found)?;
        let prepared = shell
            .prepare_direct(request.options, relative_workdir)
            .await
            .map_err(|_| remote_invalid())?;
        let mut resources = vec![resource_intent(
            prepared.relative_workdir(),
            ResourceAccess::Execute,
        )?];
        for scope in &prepared.analysis().scopes {
            resources.push(resource_intent(&scope.permission, ResourceAccess::Execute)?);
        }
        let binding = OperationBinding {
            host: request.binding.host,
            contract: fixed_contract(DIRECT_EXEC_CONTRACT_ID)?,
            argument_digest: argument_digest(&encoded)?,
        };
        remote
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::Shell(prepared),
                binding,
                OperationIntent {
                    kind: OperationKind::Execute,
                    mutating: true,
                    resources,
                },
            )
            .map_err(remote_error)
    }

    async fn prepare_scm_mutation(
        &self,
        remote: &RemoteHostState,
        request: ScmPrepareMutationRequest,
        token: &CancellationToken,
    ) -> Result<ScmPrepareMutationResponse, ErrorData> {
        remote
            .validate_host(&request.binding.host)
            .map_err(remote_error)?;
        let encoded = serde_json::to_value(&request).map_err(|_| remote_invalid())?;
        let reservation = remote
            .reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, token)
            .await
            .map_err(remote_error)?;
        let scm = self.scm.as_ref().ok_or_else(method_not_found)?;
        let prepared = scm
            .prepare_mutation_bounded(
                &request.repository_handle,
                &request.binding,
                request.mutation,
                MAX_PREPARED_OPERATION_BYTES,
                token,
            )
            .await
            .map_err(scm_error)?;
        let preview = ScmGroup::preview(&prepared).clone();
        let access = match &preview.mutation {
            ScmMutation::Discard { .. } => ResourceAccess::Write,
            ScmMutation::Stage { .. } | ScmMutation::Unstage { .. } => ResourceAccess::ReadWrite,
        };
        let mut resources = vec![ResourceIntent {
            scope: prepared.repository_resource_scope().map_err(scm_error)?,
            resource_id: prepared.repository_resource_id().clone(),
            display: DisplayText::new("repository").map_err(|_| remote_invalid())?,
            access: access.clone(),
            revision: Some(preview.revisions.repository.clone()),
        }];
        for entry in &preview.entries {
            resources.push(ResourceIntent {
                scope: prepared
                    .path_resource_scope(&entry.path)
                    .map_err(scm_error)?,
                resource_id: prepared.path_resource_id(&entry.path).map_err(scm_error)?,
                display: DisplayText::new(entry.path.as_str()).map_err(|_| remote_invalid())?,
                access: access.clone(),
                revision: Some(preview.revisions.worktree.clone()),
            });
        }
        let binding = OperationBinding {
            host: request.binding.host,
            contract: fixed_contract(SCM_MUTATION_CONTRACT_ID)?,
            argument_digest: argument_digest(&encoded)?,
        };
        let operation = remote
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::ScmMutation(prepared),
                binding,
                OperationIntent {
                    kind: OperationKind::Mutate,
                    mutating: true,
                    resources,
                },
            )
            .map_err(remote_error)?;
        Ok(ScmPrepareMutationResponse {
            version: ContractVersion::V1,
            operation,
            preview,
        })
    }

    async fn prepare_snapshot_restore(
        &self,
        remote: &RemoteHostState,
        request: SnapshotPrepareRestoreRequest,
        token: &CancellationToken,
    ) -> Result<SnapshotPrepareRestoreResponse, ErrorData> {
        remote
            .validate_host(&request.binding.host)
            .map_err(remote_error)?;
        validate_snapshot_cwd(remote, &request.binding)?;
        let encoded = serde_json::to_value(&request).map_err(|_| remote_invalid())?;
        let reservation = remote
            .reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, token)
            .await
            .map_err(remote_error)?;
        let (prepared, preview) = self
            .snapshots
            .as_ref()
            .ok_or_else(method_not_found)?
            .prepare_restore_bounded(&request.snapshot_id, MAX_PREPARED_OPERATION_BYTES, token)
            .await
            .map_err(snapshot_error)?;
        let resources = snapshot_restore_intents(&preview, prepared.pre_restore_snapshot_id())?;
        let operation = remote
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::SnapshotRestore(prepared),
                OperationBinding {
                    host: request.binding.host,
                    contract: fixed_contract(SNAPSHOT_RESTORE_CONTRACT_ID)?,
                    argument_digest: argument_digest(&encoded)?,
                },
                OperationIntent {
                    kind: OperationKind::Mutate,
                    mutating: true,
                    resources,
                },
            )
            .map_err(remote_error)?;
        Ok(SnapshotPrepareRestoreResponse {
            version: ContractVersion::V1,
            operation,
            preview,
        })
    }

    async fn prepare_snapshot_unrevert(
        &self,
        remote: &RemoteHostState,
        request: SnapshotPrepareUnrevertRequest,
        token: &CancellationToken,
    ) -> Result<SnapshotPrepareRestoreResponse, ErrorData> {
        remote
            .validate_host(&request.binding.host)
            .map_err(remote_error)?;
        validate_snapshot_cwd(remote, &request.binding)?;
        let encoded = serde_json::to_value(&request).map_err(|_| remote_invalid())?;
        let reservation = remote
            .reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, token)
            .await
            .map_err(remote_error)?;
        let (prepared, preview) = self
            .snapshots
            .as_ref()
            .ok_or_else(method_not_found)?
            .prepare_unrevert_bounded(&request.restore_id, MAX_PREPARED_OPERATION_BYTES, token)
            .await
            .map_err(snapshot_error)?;
        let resources = snapshot_restore_intents(&preview, prepared.pre_restore_snapshot_id())?;
        let operation = remote
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::SnapshotUnrevert(prepared),
                OperationBinding {
                    host: request.binding.host,
                    contract: fixed_contract(SNAPSHOT_UNREVERT_CONTRACT_ID)?,
                    argument_digest: argument_digest(&encoded)?,
                },
                OperationIntent {
                    kind: OperationKind::Mutate,
                    mutating: true,
                    resources,
                },
            )
            .map_err(remote_error)?;
        Ok(SnapshotPrepareRestoreResponse {
            version: ContractVersion::V1,
            operation,
            preview,
        })
    }

    async fn prepare_snapshot_cleanup(
        &self,
        remote: &RemoteHostState,
        request: SnapshotPrepareCleanupRequest,
        token: &CancellationToken,
    ) -> Result<SnapshotPrepareCleanupResponse, ErrorData> {
        remote
            .validate_host(&request.binding.host)
            .map_err(remote_error)?;
        validate_snapshot_cwd(remote, &request.binding)?;
        let encoded = serde_json::to_value(&request).map_err(|_| remote_invalid())?;
        let reservation = remote
            .reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, token)
            .await
            .map_err(remote_error)?;
        let (prepared, preview) = self
            .snapshots
            .as_ref()
            .ok_or_else(method_not_found)?
            .prepare_cleanup_bounded(&request.snapshot_ids, MAX_PREPARED_OPERATION_BYTES)
            .await
            .map_err(snapshot_error)?;
        let resources = vec![resource_intent(
            prepared.resource_scope(),
            ResourceAccess::Delete,
        )?];
        let operation = remote
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::SnapshotCleanup(prepared),
                OperationBinding {
                    host: request.binding.host,
                    contract: fixed_contract(SNAPSHOT_CLEANUP_CONTRACT_ID)?,
                    argument_digest: argument_digest(&encoded)?,
                },
                OperationIntent {
                    kind: OperationKind::Mutate,
                    mutating: true,
                    resources,
                },
            )
            .map_err(remote_error)?;
        Ok(SnapshotPrepareCleanupResponse {
            version: ContractVersion::V1,
            operation,
            preview,
        })
    }

    async fn execute_remote(
        &self,
        remote: &RemoteHostState,
        request: workcell_host_contract::ExecuteRequest,
        context: &RequestContext<RoleServer>,
    ) -> Result<workcell_host_contract::StatusResponse, ErrorData> {
        let standard_cancellation = context.ct.child_token();
        match remote
            .begin(&request, standard_cancellation)
            .map_err(remote_error)?
        {
            BeginExecution::Terminal(status) | BeginExecution::Running(status) => Ok(*status),
            BeginExecution::Start {
                mut operation,
                cancellation,
                lease,
            } => {
                #[cfg(unix)]
                if let PreparedRemoteOperation::TransferPublication(prepared) = operation.as_mut() {
                    prepared.bind_execution(
                        request.preparation_id.clone(),
                        request.invocation_id.clone(),
                    );
                }
                let progress = Some(ToolProgressContext {
                    mcp: context
                        .meta
                        .get_progress_token()
                        .map(|token| (context.peer.clone(), token)),
                    remote: Some(RemoteProgressContext {
                        state: remote.clone(),
                        preparation_id: request.preparation_id.clone(),
                        invocation_id: request.invocation_id.clone(),
                    }),
                });
                let server = self.clone();
                let operation_remote = remote.clone();
                let preparation_id = request.preparation_id.clone();
                let invocation_id = request.invocation_id.clone();
                let guard = RemoteExecutionGuard::new(
                    operation_remote,
                    preparation_id,
                    invocation_id,
                    lease,
                );
                let task = tokio::spawn(async move {
                    let outcome = AssertUnwindSafe(server.run_remote_execution(
                        *operation,
                        cancellation,
                        progress,
                    ))
                    .catch_unwind()
                    .await;
                    match outcome {
                        Ok(outcome) => guard.finish(outcome),
                        Err(_) => guard.indeterminate(),
                    }
                });
                let _ = task.await;
                remote
                    .status(
                        &request.preparation_id,
                        Some(&request.invocation_id),
                        &request.host,
                    )
                    .map_err(remote_error)
            }
        }
    }

    async fn run_remote_execution(
        &self,
        operation: PreparedRemoteOperation,
        cancellation: CancellationToken,
        progress: Option<ToolProgressContext>,
    ) -> StructuredOutcome {
        if cancellation.is_cancelled() {
            return cancelled_outcome(None, false);
        }
        let failure_policy = FailureEffectPolicy::for_operation(&operation);
        let execution = self.execute_prepared_operation(operation, cancellation.clone(), progress);
        tokio::pin!(execution);
        tokio::select! {
            biased;
            result = &mut execution => {
                let side_effects_possible = failure_policy.side_effects_possible(self, &result);
                if cancellation.is_cancelled()
                    && match &result {
                        Ok(result) => result.is_error == Some(true),
                        Err(_) => true,
                    }
                {
                    cancelled_outcome(
                        result.ok().and_then(|result| neutral_tool_result(result).ok()),
                        side_effects_possible,
                    )
                } else {
                    execution_outcome(result, side_effects_possible)
                }
            },
            () = cancellation.cancelled() => {
                let result = execution.await;
                let side_effects_possible = failure_policy.side_effects_possible(self, &result);
                cancelled_outcome(
                    result.ok().and_then(|result| neutral_tool_result(result).ok()),
                    side_effects_possible,
                )
            }
        }
    }

    async fn prepare_operation(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<(PreparedRemoteOperation, OperationIntent), ErrorData> {
        let invalid = || {
            ErrorData::invalid_params(
                format!("Invalid arguments for remote preparation of {name}"),
                None,
            )
        };
        let token = CancellationToken::new();
        let (operation, kind, mutating, resources) = match name {
            "file_read" => {
                let input = parse::<FileReadInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .files
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_read(input, &token)
                    .await
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::FileRead(prepared),
                    OperationKind::Read,
                    false,
                    Vec::new(),
                )
            }
            "file_glob" => {
                let input = parse::<FileGlobInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .files
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_glob(input, &token)
                    .await
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::FileGlob(prepared),
                    OperationKind::Search,
                    false,
                    Vec::new(),
                )
            }
            "file_grep" => {
                let input = parse::<FileGrepInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .files
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_grep(input, &token)
                    .await
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::FileGrep(prepared),
                    OperationKind::Search,
                    false,
                    Vec::new(),
                )
            }
            "file_write" => {
                let input = parse::<FileWriteInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .files
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_write_bounded(input, MEDIUM_PREPARATION_RESERVATION_BYTES, &token)
                    .await
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::FileWrite(prepared),
                    OperationKind::Mutate,
                    true,
                    Vec::new(),
                )
            }
            "file_edit" => {
                let input = parse::<FileEditInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .files
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_edit_bounded(input, MEDIUM_PREPARATION_RESERVATION_BYTES, &token)
                    .await
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::FileEdit(prepared),
                    OperationKind::Mutate,
                    true,
                    Vec::new(),
                )
            }
            "file_apply_patch" => {
                let input = parse::<FileApplyPatchInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .files
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_apply_patch_bounded(input, MAX_PREPARED_OPERATION_BYTES, &token)
                    .await
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::FileApplyPatch(prepared),
                    OperationKind::Mutate,
                    true,
                    Vec::new(),
                )
            }
            "file_index" => {
                let input = parse::<IndexInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .files
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_index(input, &token)
                    .await
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::FileIndex(prepared),
                    OperationKind::Inspect,
                    false,
                    Vec::new(),
                )
            }
            "code_map" => {
                let input = parse::<CodeMapInput>(arguments).map_err(|_| invalid())?;
                let graph = self.code_graph.as_ref().ok_or_else(invalid)?;
                let scope = graph
                    .inspect_scope(input.path.as_deref())
                    .await
                    .map_err(|_| invalid())?;
                let prepared = graph
                    .prepare_code_map(input, scope)
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::CodeMap(prepared),
                    OperationKind::Search,
                    false,
                    Vec::new(),
                )
            }
            "code_context" => {
                let input = parse::<CodeContextInput>(arguments).map_err(|_| invalid())?;
                let graph = self.code_graph.as_ref().ok_or_else(invalid)?;
                let scope = graph
                    .inspect_scope(input.path.as_deref())
                    .await
                    .map_err(|_| invalid())?;
                let prepared = graph
                    .prepare_code_context(input, scope)
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::CodeContext(prepared),
                    OperationKind::Search,
                    false,
                    Vec::new(),
                )
            }
            "code_refs" => {
                let input = parse::<CodeRefsInput>(arguments).map_err(|_| invalid())?;
                let graph = self.code_graph.as_ref().ok_or_else(invalid)?;
                let scope = graph
                    .inspect_scope(input.path.as_deref())
                    .await
                    .map_err(|_| invalid())?;
                let prepared = graph
                    .prepare_code_refs(input, scope)
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::CodeRefs(prepared),
                    OperationKind::Search,
                    false,
                    Vec::new(),
                )
            }
            "code_impact" => {
                let input = parse::<CodeImpactInput>(arguments).map_err(|_| invalid())?;
                let graph = self.code_graph.as_ref().ok_or_else(invalid)?;
                let scope = graph
                    .inspect_scope(input.path.as_deref())
                    .await
                    .map_err(|_| invalid())?;
                let prepared = graph
                    .prepare_code_impact(input, scope)
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::CodeImpact(prepared),
                    OperationKind::Search,
                    false,
                    Vec::new(),
                )
            }
            "code_expand" => {
                let input = parse::<CodeExpandInput>(arguments).map_err(|_| invalid())?;
                let graph = self.code_graph.as_ref().ok_or_else(invalid)?;
                let scope = graph
                    .inspect_scope(input.path.as_deref())
                    .await
                    .map_err(|_| invalid())?;
                let prepared = graph
                    .prepare_code_expand(input, scope)
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::CodeExpand(prepared),
                    OperationKind::Read,
                    false,
                    Vec::new(),
                )
            }
            "websearch" => {
                let input = parse::<WebsearchInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .web
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_websearch_operation(input)
                    .map_err(|_| invalid())?;
                let PreparedWebOperation::Websearch(prepared) = prepared else {
                    return Err(remote_invalid());
                };
                let query = prepared.permission_query();
                let query_resource = ResourceIntent {
                    scope: vec![resource_id("query/search", query).map_err(|_| remote_invalid())?],
                    resource_id: resource_id("query/search", query)
                        .map_err(|_| remote_invalid())?,
                    display: DisplayText::new(query).map_err(|_| remote_invalid())?,
                    access: ResourceAccess::Search,
                    revision: None,
                };
                (
                    PreparedRemoteOperation::Websearch(prepared),
                    OperationKind::Search,
                    false,
                    vec![
                        resource_intent("web:search", ResourceAccess::Connect)?,
                        query_resource,
                    ],
                )
            }
            "webfetch" => {
                let input = parse::<WebfetchInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .web
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare_webfetch_operation(input)
                    .map_err(|_| invalid())?;
                let PreparedWebOperation::Webfetch(prepared) = prepared else {
                    return Err(remote_invalid());
                };
                let resource = resource_intent(prepared.url().as_str(), ResourceAccess::Connect)?;
                (
                    PreparedRemoteOperation::Webfetch(prepared),
                    OperationKind::Read,
                    false,
                    vec![resource],
                )
            }
            "shell" => {
                let input = parse::<ShellInput>(arguments).map_err(|_| invalid())?;
                let shell = self.shell.as_ref().ok_or_else(invalid)?;
                let prepared = shell.prepare(input).await.map_err(|_| invalid())?;
                shell.authorize_prepared(&prepared).map_err(|_| invalid())?;
                let mut intents = vec![
                    resource_intent(prepared.relative_workdir(), ResourceAccess::Traverse)?,
                    resource_intent(prepared.command(), ResourceAccess::Execute)?,
                ];
                if let Ok(contexts) = prepared.bash_command_contexts() {
                    intents.push(resource_intent(
                        &serde_json::to_string(&contexts.assumptions).map_err(|_| invalid())?,
                        ResourceAccess::Inspect,
                    )?);
                }
                (
                    PreparedRemoteOperation::Shell(prepared),
                    OperationKind::Execute,
                    true,
                    intents,
                )
            }
            "python_execution" => {
                let input = parse::<CodeInput>(arguments).map_err(|_| invalid())?;
                let prepared = self
                    .code
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare(input)
                    .map_err(|_| invalid())?;
                (
                    PreparedRemoteOperation::PythonExecution(prepared),
                    OperationKind::Execute,
                    false,
                    vec![resource_intent(
                        ISOLATED_PYTHON_RESOURCE,
                        ResourceAccess::Execute,
                    )?],
                )
            }
            EXECUTION_ENVIRONMENT_TOOL if matches!(arguments, Value::Object(values) if values.is_empty()) =>
            {
                let prepared = self
                    .execution_environment
                    .as_ref()
                    .ok_or_else(invalid)?
                    .prepare(self.tool_group_disclosure());
                (
                    PreparedRemoteOperation::ExecutionEnvironment(prepared),
                    OperationKind::Inspect,
                    false,
                    vec![resource_intent(
                        "execution-environment",
                        ResourceAccess::Inspect,
                    )?],
                )
            }
            _ => return Err(invalid()),
        };
        let resources = if resources.is_empty() {
            operation_file_intents(&operation)?
        } else {
            resources
        };
        Ok((
            operation,
            OperationIntent {
                kind,
                mutating,
                resources,
            },
        ))
    }

    async fn execute_prepared_operation(
        &self,
        operation: PreparedRemoteOperation,
        cancellation: CancellationToken,
        progress: Option<ToolProgressContext>,
    ) -> Result<CallToolResult, ErrorData> {
        match operation {
            PreparedRemoteOperation::FileRead(prepared) => file_result(
                self.files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_read(prepared, &cancellation)
                    .await,
            ),
            PreparedRemoteOperation::FileGlob(prepared) => {
                let output = self
                    .files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_glob(prepared, &cancellation)
                    .await;
                file_search_result(output, workcell_mcp_files::fit_glob_output)
            }
            PreparedRemoteOperation::FileGrep(prepared) => {
                let output = self
                    .files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_grep(prepared, &cancellation)
                    .await;
                file_search_result(output, workcell_mcp_files::fit_grep_output)
            }
            PreparedRemoteOperation::FileWrite(prepared) => file_result(
                self.files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_write(prepared, &cancellation)
                    .await,
            ),
            PreparedRemoteOperation::FileEdit(prepared) => file_result(
                self.files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_edit(prepared, &cancellation)
                    .await,
            ),
            PreparedRemoteOperation::FileApplyPatch(prepared) => file_result(
                self.files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_patch(prepared, &cancellation)
                    .await,
            ),
            PreparedRemoteOperation::FileIndex(prepared) => {
                let output = self
                    .files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_index(prepared, &cancellation)
                    .await;
                file_index_result(output)
            }
            PreparedRemoteOperation::CodeMap(prepared) => {
                let progress = progress.map(ToolProgressContext::into_graph_sink);
                graph_result(
                    self.code_graph
                        .as_ref()
                        .ok_or_else(remote_invalid)?
                        .execute_prepared_code_map(prepared, progress.as_deref(), &cancellation)
                        .await,
                )
            }
            PreparedRemoteOperation::CodeContext(prepared) => {
                let progress = progress.map(ToolProgressContext::into_graph_sink);
                graph_result(
                    self.code_graph
                        .as_ref()
                        .ok_or_else(remote_invalid)?
                        .execute_prepared_code_context(prepared, progress.as_deref(), &cancellation)
                        .await,
                )
            }
            PreparedRemoteOperation::CodeRefs(prepared) => {
                let progress = progress.map(ToolProgressContext::into_graph_sink);
                graph_selectable_result(
                    self.code_graph
                        .as_ref()
                        .ok_or_else(remote_invalid)?
                        .execute_prepared_code_refs(prepared, progress.as_deref(), &cancellation)
                        .await,
                )
            }
            PreparedRemoteOperation::CodeImpact(prepared) => {
                let progress = progress.map(ToolProgressContext::into_graph_sink);
                graph_selectable_result(
                    self.code_graph
                        .as_ref()
                        .ok_or_else(remote_invalid)?
                        .execute_prepared_code_impact(prepared, progress.as_deref(), &cancellation)
                        .await,
                )
            }
            PreparedRemoteOperation::CodeExpand(prepared) => {
                let progress = progress.map(ToolProgressContext::into_graph_sink);
                graph_selectable_result(
                    self.code_graph
                        .as_ref()
                        .ok_or_else(remote_invalid)?
                        .execute_prepared_code_expand(prepared, progress.as_deref(), &cancellation)
                        .await,
                )
            }
            PreparedRemoteOperation::Websearch(prepared) => web_result(
                self.web
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared(PreparedWebOperation::Websearch(prepared), cancellation)
                    .await,
            ),
            PreparedRemoteOperation::Webfetch(prepared) => web_result(
                self.web
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared(PreparedWebOperation::Webfetch(prepared), cancellation)
                    .await,
            ),
            PreparedRemoteOperation::Shell(prepared) => {
                let execution = self
                    .shell
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared(
                        prepared,
                        cancellation,
                        progress.map(ToolProgressContext::into_sink),
                    )
                    .await;
                match execution {
                    Ok(Some(execution)) => {
                        typed_tool_result(&execution.output, execution.model_text)
                    }
                    Ok(None) => Ok(tool_error_result("Shell execution cancelled")),
                    Err(error) => Ok(tool_error_result(error)),
                }
            }
            PreparedRemoteOperation::PythonExecution(prepared) => {
                let execution = self
                    .code
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared(prepared, cancellation)
                    .await;
                match execution {
                    Ok(Some(execution)) => {
                        typed_tool_result(&execution.output, execution.model_text)
                    }
                    Ok(None) => Ok(tool_error_result("Code execution cancelled")),
                    Err(error) => Ok(tool_error_result(error)),
                }
            }
            PreparedRemoteOperation::ExecutionEnvironment(prepared) => {
                match self
                    .execution_environment
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared(prepared, cancellation)
                    .await
                {
                    Ok(execution) => typed_tool_result(&execution.output, execution.model_text),
                    Err(error) => Ok(tool_error_result(error)),
                }
            }
            PreparedRemoteOperation::WorkspaceMutation(prepared) => {
                match self
                    .workspace_files
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_prepared_workspace_mutation(prepared, &cancellation)
                    .await
                {
                    Ok(output) => typed_tool_result(&output, "Workspace mutation completed".into()),
                    Err(error) => operation_error_result(error.code(), error.to_string()),
                }
            }
            #[cfg(unix)]
            PreparedRemoteOperation::TransferPublication(prepared) => {
                match prepared.execute(&cancellation).await {
                    Ok(output) => {
                        typed_tool_result(&output, "Reviewed binary publication completed".into())
                    }
                    Err(error) => operation_error_result(error.code(), error.to_string()),
                }
            }
            PreparedRemoteOperation::ScmMutation(prepared) => {
                match self
                    .scm
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_mutation_tracked(prepared, &cancellation)
                    .await
                {
                    Ok(output) => typed_tool_result(&output, "SCM mutation completed".into()),
                    Err(error) => scm_mutation_error_result(error),
                }
            }
            PreparedRemoteOperation::SnapshotRestore(prepared)
            | PreparedRemoteOperation::SnapshotUnrevert(prepared) => {
                match self
                    .snapshots
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_restore(&prepared, &cancellation)
                    .await
                {
                    Ok(output) => typed_tool_result(&output, "Workspace restore completed".into()),
                    Err(error) => operation_error_result(error.code(), error.to_string()),
                }
            }
            PreparedRemoteOperation::SnapshotCleanup(prepared) => {
                match self
                    .snapshots
                    .as_ref()
                    .ok_or_else(remote_invalid)?
                    .execute_cleanup(&prepared, &cancellation)
                    .await
                {
                    Ok(output) => typed_tool_result(&output, "Snapshot cleanup completed".into()),
                    Err(error) => operation_error_result(error.code(), error.to_string()),
                }
            }
            #[cfg(test)]
            PreparedRemoteOperation::Test(_) => Err(remote_invalid()),
        }
    }

    fn tool_group_disclosure(&self) -> ToolGroupDisclosure {
        ToolGroupDisclosure {
            files: self.files.is_some(),
            web: self.web.is_some(),
            shell: self.shell.is_some(),
            code: self.code.is_some(),
            code_graph: self.code_graph.is_some(),
        }
    }

    fn canonical_tool_name(&self, requested: &str) -> Option<String> {
        self.catalog
            .iter()
            .find(|tool| tool.name.as_ref() == requested)
            .map(|tool| tool.name.to_string())
    }
}

impl ServerHandler for WorkcellServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("workcell-mcp", env!("CARGO_PKG_VERSION")),
        )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.catalog
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .cloned()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(protocol_versions(self.modern_only))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        if request.protocol_version == ProtocolVersion::V_2026_07_28 {
            return Err(ErrorData::invalid_request(
                "initialize is not valid for MCP 2026-07-28; use server/discover or per-request metadata",
                Some(serde_json::json!({"supported": self.supported_protocol_versions()})),
            ));
        }
        if self.modern_only || request.protocol_version != ProtocolVersion::V_2025_11_25 {
            return Err(ErrorData::unsupported_protocol_version(
                request.protocol_version,
                &self.supported_protocol_versions(),
            ));
        }
        context.peer.set_peer_info(request);
        let mut info = self.get_info();
        info.protocol_version = ProtocolVersion::V_2025_11_25;
        Ok(info)
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        self.validate_discover_context(&context)?;
        let mut request = InitializeRequestParams::default();
        request.protocol_version = ProtocolVersion::V_2026_07_28;
        request.capabilities = context.client_capabilities().unwrap_or_default();
        let mut info = self.get_info();
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        if let Some(descriptor) = self
            .execution_environment
            .as_ref()
            .filter(|_| requests_execution_environment(&request, &context.meta))
            .and_then(|environment| environment.discovery_descriptor(self.tool_group_disclosure()))
        {
            let mut extensions = ExtensionCapabilities::new();
            extensions.insert(
                crate::execution_environment::EXTENSION_ID.into(),
                descriptor,
            );
            info.capabilities.extensions = Some(extensions);
        }
        if let Some(remote_host) = self.remote_host.as_ref().filter(|_| {
            requests_extension(&request, &context.meta, crate::remote_host::EXTENSION_ID)
        }) {
            let extensions = info
                .capabilities
                .extensions
                .get_or_insert_with(ExtensionCapabilities::new);
            let Value::Object(descriptor) = serde_json::to_value(&*remote_host.descriptor)
                .map_err(|_| {
                    ErrorData::internal_error("remote-host descriptor unavailable", None)
                })?
            else {
                return Err(ErrorData::internal_error(
                    "remote-host descriptor unavailable",
                    None,
                ));
            };
            extensions.insert(crate::remote_host::EXTENSION_ID.into(), descriptor);
        }
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            info,
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let protocol_version = self.validate_request_context(&context)?;
        let result = ListToolsResult::with_all_items(self.catalog.to_vec());
        if protocol_version == ProtocolVersion::V_2026_07_28 {
            Ok(result.with_ttl_ms(0).with_cache_scope(CacheScope::Private))
        } else {
            Ok(result)
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.validate_request_context(&context)?;
        let Some(tool_name) = self.canonical_tool_name(request.name.as_ref()) else {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "Unknown tool",
                None,
            ));
        };
        let request_id = format!("tool_{}", Uuid::new_v4());
        let started = Instant::now();
        tracing::debug!(
            operation = "mcp.tool.started",
            request_id,
            tool = tool_name.as_str(),
            "tool call started"
        );
        let progress = context
            .meta
            .get_progress_token()
            .map(|token| ToolProgressContext {
                mcp: Some((context.peer.clone(), token)),
                remote: None,
            });
        let result = self
            .dispatch_with_context(
                &tool_name,
                Value::Object(request.arguments.unwrap_or_default()),
                context.ct,
                progress,
            )
            .await;
        let outcome = match &result {
            Ok(value) if value.is_error == Some(true) => "tool_error",
            Ok(_) => "completed",
            Err(_) => "protocol_error",
        };
        tracing::debug!(
            operation = "mcp.tool.completed",
            request_id,
            tool = tool_name.as_str(),
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            outcome,
            "tool call completed"
        );
        result.map(Into::into)
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        let reviewed_method = {
            #[cfg(unix)]
            {
                reviewed_host::is_method(request.method.as_str())
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        if !reviewed_method
            && !matches!(
                request.method.as_str(),
                PREPARE_METHOD
                    | EXECUTE_METHOD
                    | RELEASE_METHOD
                    | STATUS_METHOD
                    | CANCEL_METHOD
                    | RESOLVE_DIRECTORY_METHOD
                    | STAT_METHOD
                    | LIST_METHOD
                    | READ_TEXT_METHOD
                    | SEARCH_TEXT_METHOD
                    | WATCH_OPEN_METHOD
                    | WATCH_POLL_METHOD
                    | WATCH_CLOSE_METHOD
                    | DISCOVER_PROJECT_ASSETS_METHOD
                    | READ_PROJECT_ASSET_METHOD
                    | PREPARE_MUTATION_METHOD
                    | PREPARE_EXEC_METHOD
                    | SCM_DISCOVER_METHOD
                    | SCM_STATUS_METHOD
                    | SCM_LOG_METHOD
                    | SCM_DIFF_METHOD
                    | SCM_READ_SIDE_METHOD
                    | SCM_PREPARE_MUTATION_METHOD
                    | SNAPSHOT_CAPTURE_METHOD
                    | SNAPSHOT_INSPECT_METHOD
                    | SNAPSHOT_STATUS_METHOD
                    | SNAPSHOT_PREPARE_RESTORE_METHOD
                    | SNAPSHOT_PREPARE_UNREVERT_METHOD
                    | SNAPSHOT_ACKNOWLEDGE_METHOD
                    | SNAPSHOT_PREPARE_CLEANUP_METHOD
            )
        {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "Method not found",
                None,
            ));
        }
        self.validate_request_context(&context)?;
        let remote = self.remote_host.as_ref().ok_or_else(method_not_found)?;
        if context.protocol_version() != Some(ProtocolVersion::V_2026_07_28)
            || !requests_extension_from_context(&context, EXTENSION_ID)
        {
            return Err(method_not_found());
        }
        let params = request.params.unwrap_or(Value::Null);
        #[cfg(unix)]
        if reviewed_method {
            return self
                .reviewed_request(remote, request.method.as_str(), params, &context.ct)
                .await;
        }
        let value = match request.method.as_str() {
            PREPARE_METHOD => {
                let request = parse_custom::<PrepareRequest>(params)?;
                serde_json::to_value(self.prepare_remote(remote, request, &context.ct).await?)
            }
            EXECUTE_METHOD => {
                let request = parse_custom::<workcell_host_contract::ExecuteRequest>(params)?;
                serde_json::to_value(self.execute_remote(remote, request, &context).await?)
            }
            RELEASE_METHOD => {
                let request = parse_custom::<ReleaseRequest>(params)?;
                serde_json::to_value(
                    remote
                        .release(
                            &request.selector.preparation_id,
                            request.selector.invocation_id.as_ref(),
                            &request.selector.host,
                        )
                        .map_err(remote_error)?,
                )
            }
            STATUS_METHOD => {
                let request = parse_custom::<StatusRequest>(params)?;
                serde_json::to_value(remote.status_after(&request).map_err(remote_error)?)
            }
            CANCEL_METHOD => {
                let request = parse_custom::<CancelRequest>(params)?;
                serde_json::to_value(remote.cancel(&request).map_err(remote_error)?)
            }
            RESOLVE_DIRECTORY_METHOD => {
                let request = parse_custom::<ResolveDirectoryRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                let directory = self
                    .workspace_files
                    .as_ref()
                    .ok_or_else(method_not_found)?
                    .workspace_resolve_directory(&request.binding.cwd_handle, &request.path)
                    .await
                    .map_err(workspace_error)?;
                serde_json::to_value(workcell_host_contract::ResolveDirectoryResponse {
                    version: ContractVersion::V1,
                    directory,
                })
            }
            STAT_METHOD => {
                let request = parse_custom::<StatRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.workspace_files
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .workspace_stat(&request)
                        .await
                        .map_err(workspace_error)?,
                )
            }
            LIST_METHOD => {
                let request = parse_custom::<ListRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.workspace_files
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .workspace_list(&request, &context.ct)
                        .await
                        .map_err(workspace_error)?,
                )
            }
            READ_TEXT_METHOD => {
                let request = parse_custom::<ReadTextRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.workspace_files
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .workspace_read_text(&request, &context.ct)
                        .await
                        .map_err(workspace_error)?,
                )
            }
            SEARCH_TEXT_METHOD => {
                let request = parse_custom::<SearchTextRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.workspace_files
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .workspace_search_text(&request, &context.ct)
                        .await
                        .map_err(workspace_error)?,
                )
            }
            WATCH_OPEN_METHOD => {
                let request = parse_custom::<WatchOpenRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                let watcher = self
                    .workspace_files
                    .as_ref()
                    .ok_or_else(method_not_found)?
                    .workspace_open_watch(&request)
                    .await
                    .map_err(workspace_error)?;
                serde_json::to_value(remote.open_watch(&request, watcher).map_err(remote_error)?)
            }
            WATCH_POLL_METHOD => {
                let request = parse_custom::<WatchPollRequest>(params)?;
                serde_json::to_value(remote.poll_watch(&request).await.map_err(remote_error)?)
            }
            WATCH_CLOSE_METHOD => {
                let request = parse_custom::<WatchCloseRequest>(params)?;
                serde_json::to_value(remote.close_watch(&request).map_err(remote_error)?)
            }
            DISCOVER_PROJECT_ASSETS_METHOD => {
                let request = parse_custom::<DiscoverProjectAssetsRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.workspace_files
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .workspace_discover_project_assets(&request, &context.ct)
                        .await
                        .map_err(workspace_error)?,
                )
            }
            READ_PROJECT_ASSET_METHOD => {
                let request = parse_custom::<ReadProjectAssetRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.workspace_files
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .workspace_read_project_asset(&request, &context.ct)
                        .await
                        .map_err(workspace_error)?,
                )
            }
            PREPARE_MUTATION_METHOD => {
                let request = parse_custom::<PrepareMutationRequest>(params)?;
                serde_json::to_value(
                    self.prepare_workspace_mutation(remote, request, &context.ct)
                        .await?,
                )
            }
            PREPARE_EXEC_METHOD => {
                let request = parse_custom::<PrepareExecRequest>(params)?;
                serde_json::to_value(
                    self.prepare_direct_exec(remote, request, &context.ct)
                        .await?,
                )
            }
            SCM_DISCOVER_METHOD => {
                let request = parse_custom::<ScmDiscoverRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.scm
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .discover(&request, &context.ct)
                        .await
                        .map_err(scm_error)?,
                )
            }
            SCM_STATUS_METHOD => {
                let request = parse_custom::<ScmStatusRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.scm
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .status(&request, &context.ct)
                        .await
                        .map_err(scm_error)?,
                )
            }
            SCM_LOG_METHOD => {
                let request = parse_custom::<ScmLogRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.scm
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .log(&request, &context.ct)
                        .await
                        .map_err(scm_error)?,
                )
            }
            SCM_DIFF_METHOD => {
                let request = parse_custom::<ScmDiffRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.scm
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .diff(&request, &context.ct)
                        .await
                        .map_err(scm_error)?,
                )
            }
            SCM_READ_SIDE_METHOD => {
                let request = parse_custom::<ScmReadSideRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                serde_json::to_value(
                    self.scm
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .read_side(&request, &context.ct)
                        .await
                        .map_err(scm_error)?,
                )
            }
            SCM_PREPARE_MUTATION_METHOD => {
                let request = parse_custom::<ScmPrepareMutationRequest>(params)?;
                serde_json::to_value(
                    self.prepare_scm_mutation(remote, request, &context.ct)
                        .await?,
                )
            }
            SNAPSHOT_CAPTURE_METHOD => {
                let request = parse_custom::<SnapshotCaptureRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                validate_snapshot_cwd(remote, &request.binding)?;
                serde_json::to_value(
                    self.snapshots
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .capture(&request.checkpoint_id, &context.ct)
                        .await
                        .map_err(snapshot_error)?,
                )
            }
            SNAPSHOT_INSPECT_METHOD => {
                let request = parse_custom::<SnapshotInspectRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                validate_snapshot_cwd(remote, &request.binding)?;
                serde_json::to_value(
                    self.snapshots
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .inspect(
                            &request.snapshot_id,
                            request.page_size,
                            request.cursor.as_ref(),
                        )
                        .await
                        .map_err(snapshot_error)?,
                )
            }
            SNAPSHOT_STATUS_METHOD => {
                let request = parse_custom::<SnapshotStatusRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                validate_snapshot_cwd(remote, &request.binding)?;
                serde_json::to_value(
                    self.snapshots
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .status(&request.restore_id)
                        .map_err(snapshot_error)?,
                )
            }
            SNAPSHOT_PREPARE_RESTORE_METHOD => {
                let request = parse_custom::<SnapshotPrepareRestoreRequest>(params)?;
                serde_json::to_value(
                    self.prepare_snapshot_restore(remote, request, &context.ct)
                        .await?,
                )
            }
            SNAPSHOT_PREPARE_UNREVERT_METHOD => {
                let request = parse_custom::<SnapshotPrepareUnrevertRequest>(params)?;
                serde_json::to_value(
                    self.prepare_snapshot_unrevert(remote, request, &context.ct)
                        .await?,
                )
            }
            SNAPSHOT_ACKNOWLEDGE_METHOD => {
                let request = parse_custom::<SnapshotAcknowledgeRequest>(params)?;
                remote
                    .validate_workspace_host(&request.binding.host)
                    .map_err(remote_error)?;
                validate_snapshot_cwd(remote, &request.binding)?;
                serde_json::to_value(
                    self.snapshots
                        .as_ref()
                        .ok_or_else(method_not_found)?
                        .acknowledge(&request.restore_id)
                        .await
                        .map_err(snapshot_error)?,
                )
            }
            SNAPSHOT_PREPARE_CLEANUP_METHOD => {
                let request = parse_custom::<SnapshotPrepareCleanupRequest>(params)?;
                serde_json::to_value(
                    self.prepare_snapshot_cleanup(remote, request, &context.ct)
                        .await?,
                )
            }
            _ => unreachable!("known custom method"),
        }
        .map_err(|_| ErrorData::internal_error("remote operation response unavailable", None))?;
        Ok(CustomResult::new(value))
    }
}

struct RemoteExecutionGuard {
    state: RemoteHostState,
    preparation_id: Identifier,
    invocation_id: Identifier,
    _lease: crate::remote_host::ExecutionLease,
    armed: bool,
}

enum FailureEffectPolicy {
    None,
    FileMutation,
    Shell,
    WorkspaceMutation,
    ScmMutation,
    SnapshotRestore,
    SnapshotCleanup,
    #[cfg(unix)]
    TransferPublication,
}

impl FailureEffectPolicy {
    fn for_operation(operation: &PreparedRemoteOperation) -> Self {
        match operation {
            #[cfg(unix)]
            PreparedRemoteOperation::TransferPublication(_) => Self::TransferPublication,
            PreparedRemoteOperation::FileWrite(_)
            | PreparedRemoteOperation::FileEdit(_)
            | PreparedRemoteOperation::FileApplyPatch(_) => Self::FileMutation,
            PreparedRemoteOperation::Shell(_) => Self::Shell,
            PreparedRemoteOperation::WorkspaceMutation(_) => Self::WorkspaceMutation,
            PreparedRemoteOperation::ScmMutation(_) => Self::ScmMutation,
            PreparedRemoteOperation::SnapshotRestore(_)
            | PreparedRemoteOperation::SnapshotUnrevert(_) => Self::SnapshotRestore,
            PreparedRemoteOperation::SnapshotCleanup(_) => Self::SnapshotCleanup,
            _ => Self::None,
        }
    }

    fn side_effects_possible(
        &self,
        _server: &WorkcellServer,
        result: &Result<CallToolResult, ErrorData>,
    ) -> bool {
        let error_code = result_error_code(result);
        let failed = result
            .as_ref()
            .map_or(true, |result| result.is_error == Some(true));
        if !failed {
            return !matches!(self, Self::None);
        }
        match self {
            Self::None => false,
            #[cfg(unix)]
            Self::TransferPublication => matches!(error_code, Some("transferIndeterminate") | None),
            Self::FileMutation => true,
            Self::Shell => true,
            Self::WorkspaceMutation => {
                matches!(error_code, Some("partial_failure")) || error_code.is_none()
            }
            Self::ScmMutation => result_effect_hint(result).unwrap_or({
                matches!(
                    error_code,
                    Some("cancelled" | "timed_out" | "operation_failed") | None
                )
            }),
            Self::SnapshotRestore => true,
            Self::SnapshotCleanup => {
                matches!(error_code, Some("cancelled" | "operation_failed") | None)
            }
        }
    }
}

impl RemoteExecutionGuard {
    fn new(
        state: RemoteHostState,
        preparation_id: Identifier,
        invocation_id: Identifier,
        lease: crate::remote_host::ExecutionLease,
    ) -> Self {
        Self {
            state,
            preparation_id,
            invocation_id,
            _lease: lease,
            armed: true,
        }
    }

    fn finish(mut self, outcome: StructuredOutcome) {
        self.state
            .finish(&self.preparation_id, &self.invocation_id, outcome);
        self.armed = false;
    }

    fn indeterminate(mut self) {
        self.state
            .finish_indeterminate(&self.preparation_id, &self.invocation_id);
        self.armed = false;
    }
}

impl Drop for RemoteExecutionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state
                .finish_indeterminate(&self.preparation_id, &self.invocation_id);
        }
    }
}

struct ToolProgressContext {
    mcp: Option<(Peer<RoleServer>, ProgressToken)>,
    remote: Option<RemoteProgressContext>,
}

impl ToolProgressContext {
    fn into_sink(self) -> Arc<dyn ShellProgressSink> {
        Arc::new(CombinedShellProgressSink {
            mcp: self.mcp.map(|(peer, token)| mcp_progress_sink(peer, token)),
            remote: self.remote,
        })
    }

    fn into_graph_sink(self) -> Arc<dyn GraphProgressSink> {
        Arc::new(CombinedGraphProgressSink {
            mcp: self.mcp,
            remote: self.remote,
            sequence: AtomicU64::new(1),
        })
    }
}

struct RemoteProgressContext {
    state: RemoteHostState,
    preparation_id: Identifier,
    invocation_id: Identifier,
}

struct CombinedShellProgressSink {
    mcp: Option<Arc<dyn ShellProgressSink>>,
    remote: Option<RemoteProgressContext>,
}

struct CombinedGraphProgressSink {
    mcp: Option<(Peer<RoleServer>, ProgressToken)>,
    remote: Option<RemoteProgressContext>,
    sequence: AtomicU64,
}

#[async_trait::async_trait]
impl ShellProgressSink for CombinedShellProgressSink {
    async fn publish(&self, chunk: ShellProgressChunk) -> Result<(), String> {
        if let Some(mcp) = &self.mcp {
            mcp.publish(chunk.clone()).await?;
        }
        if let Some(remote) = &self.remote {
            remote.state.capture_progress(
                &remote.preparation_id,
                &remote.invocation_id,
                chunk.sequence,
                match chunk.stream {
                    workcell_mcp_shell::ShellStream::Stdout => "stdout",
                    workcell_mcp_shell::ShellStream::Stderr => "stderr",
                },
                chunk.text,
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl GraphProgressSink for CombinedGraphProgressSink {
    async fn publish(&self, progress: GraphProgress) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let message = format!(
            "code graph {}: {} files",
            progress.phase.label(),
            progress.files
        );
        if let Some((peer, token)) = &self.mcp {
            let notification = ProgressNotificationParam::new(token.clone(), sequence as f64)
                .with_message(message.clone());
            let _ = peer.notify_progress(notification).await;
        }
        if let Some(remote) = &self.remote {
            remote.state.capture_progress(
                &remote.preparation_id,
                &remote.invocation_id,
                sequence,
                progress.phase.label(),
                message,
            );
        }
    }
}

fn requests_execution_environment(
    request: &InitializeRequestParams,
    meta: &RequestMetaObject,
) -> bool {
    request
        .capabilities
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get(crate::execution_environment::EXTENSION_ID))
        .or_else(|| {
            meta.0
                .0
                .get(crate::execution_environment::EXTENSION_ID)?
                .as_object()
        })
        .and_then(|settings| settings.get("versions"))
        .and_then(Value::as_array)
        .is_some_and(|versions| versions.iter().any(|version| version == "v1"))
}

fn requests_extension(
    request: &InitializeRequestParams,
    meta: &RequestMetaObject,
    extension_id: &str,
) -> bool {
    request
        .capabilities
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get(extension_id))
        .or_else(|| meta.0.0.get(extension_id)?.as_object())
        .and_then(|settings| settings.get("versions"))
        .and_then(Value::as_array)
        .is_some_and(|versions| versions.iter().any(|version| version == "v1"))
}

fn requests_extension_from_context(
    context: &RequestContext<RoleServer>,
    extension_id: &str,
) -> bool {
    context
        .client_capabilities()
        .and_then(|capabilities| capabilities.extensions)
        .as_ref()
        .and_then(|extensions| extensions.get(extension_id))
        .or_else(|| context.meta.0.0.get(extension_id)?.as_object())
        .and_then(|settings| settings.get("versions"))
        .and_then(Value::as_array)
        .is_some_and(|versions| versions.iter().any(|version| version == "v1"))
}

fn parse_custom<T: DeserializeOwned>(params: Value) -> Result<T, ErrorData> {
    serde_json::from_value(params)
        .map_err(|_| remote_error(crate::remote_host::RemoteOperationError::InvalidRequest))
}

fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(arguments)
}

fn cwd_tool_arguments(name: &str, mut arguments: Value, cwd: &str) -> Result<Value, ErrorData> {
    if cwd == "." {
        return Ok(arguments);
    }
    let object = arguments.as_object_mut().ok_or_else(remote_invalid)?;
    let path_key = match name {
        "file_read" | "file_write" | "file_edit" => Some(("filePath", false)),
        "file_glob" | "file_grep" | "file_index" | "code_map" | "code_context" | "code_refs"
        | "code_impact" | "code_expand" => Some(("path", true)),
        "shell" => Some(("workdir", true)),
        _ => None,
    };
    if let Some((key, optional)) = path_key {
        let path = match object.get(key) {
            Some(Value::String(path)) => path.as_str(),
            None if optional => ".",
            _ => return Err(remote_invalid()),
        };
        object.insert(key.into(), Value::String(cwd_tool_path(cwd, path)));
    }
    if name == "file_apply_patch" {
        let patch = object
            .get("patchText")
            .and_then(Value::as_str)
            .ok_or_else(remote_invalid)?;
        let mut rebased = String::new();
        for line in patch.split_inclusive('\n') {
            let directive = [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .into_iter()
            .find(|prefix| line.starts_with(prefix));
            if let Some(prefix) = directive {
                let path = line[prefix.len()..].trim_end_matches(['\r', '\n']);
                rebased.push_str(prefix);
                rebased.push_str(&cwd_tool_path(cwd, path));
                if line.ends_with('\n') {
                    rebased.push('\n');
                }
            } else {
                rebased.push_str(line);
            }
            if rebased.len() > MAX_ARGUMENT_BYTES {
                return Err(remote_invalid());
            }
        }
        object.insert("patchText".into(), Value::String(rebased));
    }
    Ok(arguments)
}

fn cwd_tool_path(cwd: &str, path: &str) -> String {
    if Path::new(path).is_absolute() {
        path.to_owned()
    } else if path.is_empty() || path == "." {
        cwd.to_owned()
    } else {
        format!("{cwd}/{path}")
    }
}

fn preparation_reservation_bytes(name: &str) -> usize {
    match name {
        "file_apply_patch" | "shell" => LARGE_PREPARATION_RESERVATION_BYTES,
        "file_grep" | "file_write" | "file_edit" => MEDIUM_PREPARATION_RESERVATION_BYTES,
        _ => SMALL_PREPARATION_RESERVATION_BYTES,
    }
}

fn contract_binding(tool: &Tool) -> Result<ContractBinding, ErrorData> {
    let contract = tool
        .meta
        .as_ref()
        .and_then(|meta| meta.0.get(workcell_tool_contract::CONTRACT_METADATA_KEY))
        .and_then(Value::as_object)
        .ok_or_else(|| ErrorData::internal_error("tool contract unavailable", None))?;
    let value = |name| {
        contract
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorData::internal_error("tool contract unavailable", None))
            .and_then(|value| {
                Identifier::new(value)
                    .map_err(|_| ErrorData::internal_error("tool contract unavailable", None))
            })
    };
    Ok(ContractBinding {
        id: value("id")?,
        version: value("version")?,
        result_version: value("resultVersion")?,
    })
}

fn fixed_contract(id: &str) -> Result<ContractBinding, ErrorData> {
    Ok(ContractBinding {
        id: Identifier::new(id).map_err(|_| remote_invalid())?,
        version: Identifier::new("v1").map_err(|_| remote_invalid())?,
        result_version: Identifier::new("v1").map_err(|_| remote_invalid())?,
    })
}

fn argument_digest(arguments: &Value) -> Result<Revision, ErrorData> {
    let revision = CatalogRevision::for_serializable(arguments)
        .map_err(|_| ErrorData::invalid_params("arguments cannot be encoded", None))?;
    Revision::new(revision.as_str())
        .map_err(|_| ErrorData::internal_error("argument digest unavailable", None))
}

fn durable_workspace_binding(
    configuration: &RemoteHostConfiguration,
) -> Result<Identifier, fmt::Error> {
    let mut digest = Sha256::new();
    digest.update(b"workcell-workspace-binding-v1\0");
    for component in [
        configuration.server_id.as_str(),
        configuration.workspace_id.as_str(),
        configuration.workspace_generation.as_str(),
        RESOURCE_NAMESPACE_VERSION,
        configuration.root_project_id.as_str(),
        configuration.principal_id.as_str(),
    ] {
        digest.update(component.len().to_be_bytes());
        digest.update(component.as_bytes());
    }
    let mut value = "workspace_".to_owned();
    for byte in digest.finalize() {
        write!(value, "{byte:02x}")?;
    }
    Identifier::new(value).map_err(|_| fmt::Error)
}

fn file_intent(resource: &FileResource) -> Result<ResourceIntent, ErrorData> {
    let access = match resource.access {
        FileResourceAccess::Read => ResourceAccess::Read,
        FileResourceAccess::Traverse => ResourceAccess::Traverse,
        FileResourceAccess::Write => ResourceAccess::Write,
        FileResourceAccess::ReadWrite => ResourceAccess::ReadWrite,
        FileResourceAccess::Delete => ResourceAccess::Delete,
    };
    Ok(ResourceIntent {
        scope: resource.resource_scope().map_err(workspace_error)?,
        resource_id: resource.resource_id().map_err(workspace_error)?,
        display: DisplayText::new(format!("file:{}", resource.requested_path))
            .map_err(|_| remote_invalid())?,
        access,
        revision: None,
    })
}

fn file_intents(resources: &[FileResource]) -> Result<Vec<ResourceIntent>, ErrorData> {
    resources.iter().map(file_intent).collect()
}

fn operation_file_intents(
    operation: &PreparedRemoteOperation,
) -> Result<Vec<ResourceIntent>, ErrorData> {
    match operation {
        PreparedRemoteOperation::FileRead(prepared) => file_intents(prepared.resources()),
        PreparedRemoteOperation::FileGlob(prepared) => file_intents(prepared.resources()),
        PreparedRemoteOperation::FileGrep(prepared) => file_intents(prepared.resources()),
        PreparedRemoteOperation::FileWrite(prepared) => file_intents(prepared.resources()),
        PreparedRemoteOperation::FileEdit(prepared) => file_intents(prepared.resources()),
        PreparedRemoteOperation::FileApplyPatch(prepared) => file_intents(prepared.resources()),
        PreparedRemoteOperation::FileIndex(prepared) => file_intents(prepared.resources()),
        PreparedRemoteOperation::CodeMap(prepared) => {
            file_intents(std::slice::from_ref(prepared.scope()))
        }
        PreparedRemoteOperation::CodeContext(prepared) => {
            file_intents(std::slice::from_ref(prepared.scope()))
        }
        PreparedRemoteOperation::CodeRefs(prepared) => {
            file_intents(std::slice::from_ref(prepared.scope()))
        }
        PreparedRemoteOperation::CodeImpact(prepared) => {
            file_intents(std::slice::from_ref(prepared.scope()))
        }
        PreparedRemoteOperation::CodeExpand(prepared) => {
            file_intents(std::slice::from_ref(prepared.scope()))
        }
        _ => Ok(Vec::new()),
    }
}

fn validate_snapshot_cwd(
    remote: &RemoteHostState,
    binding: &WorkspaceRequestBinding,
) -> Result<(), ErrorData> {
    if binding.cwd_handle != remote.binding.cwd_handle {
        return Err(workspace_error(WorkspaceError::StaleCwd));
    }
    Ok(())
}

fn snapshot_restore_intents(
    preview: &SnapshotRestorePreview,
    pre_restore_snapshot_id: &str,
) -> Result<Vec<ResourceIntent>, ErrorData> {
    let mut resources = preview
        .changes
        .iter()
        .map(|change| {
            Ok(ResourceIntent {
                scope: workcell_mcp_files::root_relative_resource_scope(
                    RootResourceKind::Path,
                    change.path.as_str(),
                )
                .map_err(workspace_error)?,
                resource_id: change.resource_id.clone(),
                display: DisplayText::new(change.path.as_str()).map_err(|_| remote_invalid())?,
                access: if change.target_revision.is_some() {
                    ResourceAccess::Write
                } else {
                    ResourceAccess::Delete
                },
                revision: change.current_revision.clone(),
            })
        })
        .collect::<Result<Vec<_>, ErrorData>>()?;
    for directory in &preview.created_directories {
        resources.push(ResourceIntent {
            scope: workcell_mcp_files::root_relative_resource_scope(
                RootResourceKind::Path,
                directory.as_str(),
            )
            .map_err(workspace_error)?,
            resource_id: root_relative_resource_id(RootResourceKind::Path, directory.as_str())
                .map_err(workspace_error)?,
            display: DisplayText::new(format!("file:{}", directory.as_str()))
                .map_err(|_| remote_invalid())?,
            access: ResourceAccess::Write,
            revision: None,
        });
    }
    resources.push(resource_intent(
        &format!("snapshot-store:pre-restore:{pre_restore_snapshot_id}:manifest-and-blobs"),
        ResourceAccess::Write,
    )?);
    resources.push(resource_intent(
        &format!("snapshot-store:journal:{}", preview.restore_id.as_str()),
        ResourceAccess::Write,
    )?);
    resources.push(resource_intent(
        "snapshot-store:acknowledged-restore-journals",
        ResourceAccess::Delete,
    )?);
    Ok(resources)
}

fn resource_intent(scope: &str, access: ResourceAccess) -> Result<ResourceIntent, ErrorData> {
    Ok(ResourceIntent {
        scope: vec![resource_id("resource", scope).map_err(|_| remote_invalid())?],
        resource_id: resource_id("resource", scope).map_err(|_| remote_invalid())?,
        display: DisplayText::new(scope).map_err(|_| remote_invalid())?,
        access,
        revision: None,
    })
}

fn resource_id(namespace: &str, resource: &str) -> Result<ResourceId, fmt::Error> {
    let mut digest = Sha256::new();
    digest.update(b"workcell-resource-v1\0");
    digest.update(namespace.as_bytes());
    digest.update(b"\0");
    digest.update(resource.as_bytes());
    let mut value = "sha256:".to_owned();
    for byte in digest.finalize() {
        write!(value, "{byte:02x}")?;
    }
    ResourceId::new(value).map_err(|_| fmt::Error)
}

fn file_result<T>(
    result: Result<T, workcell_mcp_files::FilesystemError>,
) -> Result<CallToolResult, ErrorData>
where
    T: Serialize + FileModelText,
{
    match result {
        Ok(output) => typed_tool_result(&output, FileModelText::model_text(&output).into_owned()),
        Err(error) => Ok(tool_error_result(error)),
    }
}

fn file_search_result<T>(
    result: Result<T, workcell_mcp_files::FilesystemError>,
    fit: impl FnOnce(T) -> Result<T, serde_json::Error>,
) -> Result<CallToolResult, ErrorData>
where
    T: Serialize + FileModelText,
{
    match result {
        Ok(output) => {
            let output = fit(output).map_err(|_| {
                ErrorData::internal_error("Failed to serialize filesystem tool result", None)
            })?;
            typed_tool_result(&output, FileModelText::model_text(&output).into_owned())
        }
        Err(error) => Ok(tool_error_result(error)),
    }
}

fn file_index_result(
    result: Result<workcell_mcp_files::IndexOutput, workcell_mcp_files::FilesystemError>,
) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(output) => {
            let output = workcell_mcp_files::fit_index_output(output)
                .map_err(|_| ErrorData::internal_error("Failed to serialize index result", None))?;
            let model_text = output.model_text().to_owned();
            typed_tool_result(&output, model_text)
        }
        Err(error) => Ok(tool_error_result(error)),
    }
}

fn graph_result<T>(
    result: Result<T, workcell_mcp_code_graph::CodeGraphError>,
) -> Result<CallToolResult, ErrorData>
where
    T: Serialize + GraphModelText + Clone + Shrinkable,
{
    match result {
        Ok(output) => {
            let output = workcell_mcp_code_graph::fit(output);
            typed_tool_result(&output, GraphModelText::model_text(&output).into_owned())
        }
        Err(error) => Ok(tool_error_result(error)),
    }
}

fn graph_selectable_result<T>(
    result: Result<Result<T, SelectorRefusal>, workcell_mcp_code_graph::CodeGraphError>,
) -> Result<CallToolResult, ErrorData>
where
    T: Serialize + GraphModelText + Clone + Shrinkable,
{
    match result {
        Ok(Ok(output)) => {
            let output = workcell_mcp_code_graph::fit(output);
            typed_tool_result(&output, GraphModelText::model_text(&output).into_owned())
        }
        Ok(Err(refusal)) => {
            typed_tool_result(&refusal, GraphModelText::model_text(&refusal).into_owned())
        }
        Err(error) => Ok(tool_error_result(error)),
    }
}

fn web_result(
    result: Result<WebOperationExecution, workcell_mcp_web::WebOperationError>,
) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(WebOperationExecution::Websearch(execution)) => {
            typed_tool_result(&execution.output, execution.model_text)
        }
        Ok(WebOperationExecution::Webfetch(execution)) => {
            typed_tool_result(&execution.output, execution.model_text)
        }
        Err(error) => Ok(tool_error_result(error)),
    }
}

fn typed_tool_result(
    output: &impl Serialize,
    model_text: String,
) -> Result<CallToolResult, ErrorData> {
    let structured = serde_json::to_value(output)
        .map_err(|_| ErrorData::internal_error("Failed to serialize tool result", None))?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(model_text)];
    result.structured_content = Some(structured);
    Ok(result)
}

fn tool_error_result(error: impl ToString) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

fn neutral_tool_result(result: CallToolResult) -> Result<ToolResultEnvelope, ErrorData> {
    let content = result
        .content
        .into_iter()
        .map(|content| match content {
            ContentBlock::Text(content) => ToolResultText::new(content.text)
                .map(|text| ToolResultContent::Text { text })
                .map_err(|_| {
                    ErrorData::internal_error("remote operation result exceeds its limit", None)
                }),
            _ => Err(ErrorData::internal_error(
                "remote operation result contains unsupported content",
                None,
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    ToolResultEnvelope::new(
        content,
        result.structured_content,
        result.is_error.unwrap_or(false),
    )
    .map_err(|_| ErrorData::internal_error("remote operation result exceeds its limit", None))
}

fn execution_outcome(
    result: Result<CallToolResult, ErrorData>,
    side_effects_possible: bool,
) -> StructuredOutcome {
    match result {
        Ok(result) => {
            let kind = if result.is_error == Some(true) {
                OutcomeKind::Failed
            } else {
                OutcomeKind::Completed
            };
            let failed = kind == OutcomeKind::Failed;
            match neutral_tool_result(result) {
                Ok(result) => StructuredOutcome {
                    kind,
                    side_effects_possible: failed && side_effects_possible,
                    result: Some(result),
                    error: None,
                },
                Err(_) => failed_outcome(
                    "result_encoding_error",
                    "remote operation result could not be retained",
                    side_effects_possible,
                ),
            }
        }
        Err(error) => failed_outcome(
            "tool_protocol_error",
            error.message.as_ref(),
            side_effects_possible,
        ),
    }
}

fn cancelled_outcome(
    result: Option<ToolResultEnvelope>,
    side_effects_possible: bool,
) -> StructuredOutcome {
    StructuredOutcome {
        kind: OutcomeKind::Cancelled,
        side_effects_possible,
        result,
        error: None,
    }
}

fn failed_outcome(code: &str, message: &str, side_effects_possible: bool) -> StructuredOutcome {
    StructuredOutcome {
        kind: OutcomeKind::Failed,
        side_effects_possible,
        result: None,
        error: Some(workcell_host_contract::OperationError {
            code: Identifier::new(code)
                .unwrap_or_else(|_| Identifier::new("operation_error").expect("constant")),
            message: ErrorText::new(message)
                .unwrap_or_else(|_| ErrorText::new("tool protocol error").expect("constant")),
        }),
    }
}

fn remote_invalid() -> ErrorData {
    remote_error(crate::remote_host::RemoteOperationError::InvalidRequest)
}

fn method_not_found() -> ErrorData {
    ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Method not found", None)
}

fn remote_error(error: crate::remote_host::RemoteOperationError) -> ErrorData {
    ErrorData::invalid_params(
        error.message(),
        Some(serde_json::json!({"code": error.code()})),
    )
}

fn workspace_error(error: WorkspaceError) -> ErrorData {
    let code = error.code();
    let message = match error {
        WorkspaceError::InvalidRequest => "workspace request is invalid",
        WorkspaceError::StaleCwd => "workspace cwd handle is unknown or stale",
        WorkspaceError::InvalidCursor => "workspace cursor is invalid",
        WorkspaceError::StaleCursor => "workspace cursor is stale",
        WorkspaceError::StaleResource => "workspace resource revision is stale",
        WorkspaceError::WatchUnavailable => "workspace watch backend is unavailable",
        WorkspaceError::RepositoryUnavailable => "workspace repository is unavailable",
        WorkspaceError::UnsupportedRepository => "workspace repository layout is unsupported",
        WorkspaceError::RolledBack(_) => "workspace mutation was rolled back",
        WorkspaceError::PartialFailure(_) => "workspace mutation rollback was incomplete",
        WorkspaceError::Filesystem(_) => "workspace filesystem operation failed",
    };
    ErrorData::invalid_params(message, Some(serde_json::json!({"code": code})))
}

fn scm_error(error: ScmError) -> ErrorData {
    ErrorData::invalid_params(
        error.to_string(),
        Some(serde_json::json!({"code": error.code()})),
    )
}

fn snapshot_error(error: SnapshotError) -> ErrorData {
    ErrorData::invalid_params(
        error.to_string(),
        Some(serde_json::json!({"code": error.code()})),
    )
}

fn scm_mutation_error_result(
    failure: workcell_workspace_scm::ScmMutationFailure,
) -> Result<CallToolResult, ErrorData> {
    let error = failure.error();
    let structured = serde_json::json!({
        "error": {
            "code": error.code(),
            "message": error.to_string(),
        },
        "sideEffectsPossible": failure.side_effects_possible(),
    });
    let mut result = typed_tool_result(&structured, error.to_string())?;
    result.is_error = Some(true);
    Ok(result)
}

fn operation_error_result(
    code: &str,
    message: impl Into<String>,
) -> Result<CallToolResult, ErrorData> {
    let message = message.into();
    let structured = serde_json::json!({
        "error": {
            "code": code,
            "message": message,
        }
    });
    let mut result = typed_tool_result(&structured, message)?;
    result.is_error = Some(true);
    Ok(result)
}

fn result_error_code(result: &Result<CallToolResult, ErrorData>) -> Option<&str> {
    result
        .as_ref()
        .ok()?
        .structured_content
        .as_ref()?
        .get("error")?
        .get("code")?
        .as_str()
}

fn result_effect_hint(result: &Result<CallToolResult, ErrorData>) -> Option<bool> {
    result
        .as_ref()
        .ok()?
        .structured_content
        .as_ref()?
        .get("sideEffectsPossible")?
        .as_bool()
}

fn compose_catalog(
    groups: impl IntoIterator<Item = Vec<Tool>>,
) -> Result<Vec<Tool>, ServerBuildError> {
    let mut names = HashSet::new();
    let mut catalog = Vec::new();
    for tool in groups.into_iter().flatten() {
        if !names.insert(tool.name.to_string()) {
            return Err(ServerBuildError::DuplicateToolName);
        }
        catalog.push(tool);
    }
    Ok(catalog)
}

fn compose_specs(
    groups: impl IntoIterator<Item = Vec<ToolSpec>>,
) -> Result<Vec<ToolSpec>, ServerBuildError> {
    let mut names = HashSet::new();
    let mut specs = Vec::new();
    for spec in groups.into_iter().flatten() {
        if !names.insert(spec.name) {
            return Err(ServerBuildError::DuplicateToolName);
        }
        specs.push(spec);
    }
    Ok(specs)
}

fn current_utc_year() -> i32 {
    let days_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() / 86_400);
    year_from_unix_days(i64::try_from(days_since_epoch).unwrap_or(i64::MAX))
}

fn process_instance_id() -> Arc<str> {
    PROCESS_INSTANCE_ID
        .get_or_init(|| format!("workcell_{}", Uuid::new_v4()).into())
        .clone()
}

fn year_from_unix_days(days: i64) -> i32 {
    let shifted = days.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    if month_prime >= 10 {
        year += 1;
    }
    i32::try_from(year).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use workcell_host_contract::WorkspacePath;
    use workcell_mcp_code::{WORKER_FILE_NAME, WorkerSource};
    use workcell_mcp_files::catalog as file_catalog;
    use workcell_mcp_web::{WebsearchExecutionConfiguration, catalog as web_catalog};

    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_cwd_rebases_paths_and_patch_directives_not_content() {
        assert_eq!(
            cwd_tool_arguments("shell", json!({"command":"pwd"}), "nested").unwrap(),
            json!({"command":"pwd", "workdir":"nested"})
        );
        assert_eq!(
            cwd_tool_arguments("file_read", json!({"filePath":"file"}), "nested").unwrap(),
            json!({"filePath":"nested/file"})
        );
        assert_eq!(
            cwd_tool_arguments("file_read", json!({"filePath":"/absolute"}), "nested").unwrap(),
            json!({"filePath":"/absolute"})
        );
        let patch = "*** Begin Patch\n*** Add File: a\n+*** Update File: content\n*** Update File: b\n*** Move to: c\n@@\n-old\n+new\n*** Delete File: d\n*** End Patch";
        let expected = "*** Begin Patch\n*** Add File: nested/a\n+*** Update File: content\n*** Update File: nested/b\n*** Move to: nested/c\n@@\n-old\n+new\n*** Delete File: nested/d\n*** End Patch";
        assert_eq!(
            cwd_tool_arguments("file_apply_patch", json!({"patchText":patch}), "nested").unwrap(),
            json!({"patchText":expected})
        );
    }

    /// Tool configuration for cases that select no groups, so no group is actually constructed.
    fn test_tools() -> ToolConfiguration<'static> {
        ToolConfiguration {
            allow_write: false,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: crate::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: None,
            snapshot_exclusions: &[],
        }
    }

    #[tokio::test]
    async fn mutation_failure_policy_preserves_proven_clean_failures_and_marks_uncertainty() {
        let root = tempfile::tempdir().unwrap();
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[],
            ServerBehavior::default(),
            test_tools(),
        )
        .await
        .unwrap();
        for (policy, code, expected) in [
            (FailureEffectPolicy::FileMutation, "operation_failed", true),
            (FailureEffectPolicy::WorkspaceMutation, "rolled_back", false),
            (
                FailureEffectPolicy::WorkspaceMutation,
                "partial_failure",
                true,
            ),
            (
                FailureEffectPolicy::ScmMutation,
                "stale_prepared_operation",
                false,
            ),
            (FailureEffectPolicy::ScmMutation, "operation_failed", true),
            (FailureEffectPolicy::SnapshotRestore, "conflict", true),
            (FailureEffectPolicy::SnapshotCleanup, "conflict", false),
            (FailureEffectPolicy::SnapshotCleanup, "cancelled", true),
        ] {
            let result = operation_error_result(code, code).unwrap();
            assert_eq!(
                policy.side_effects_possible(&server, &Ok(result)),
                expected,
                "unexpected effect certainty for {code}"
            );
        }
    }

    #[tokio::test]
    async fn two_file_patch_failure_is_effectful_while_pre_dispatch_cancellation_is_clean() {
        let root = tempfile::tempdir().unwrap();
        let mut tools = test_tools();
        tools.allow_write = true;
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[ToolGroup::Files],
            ServerBehavior::default(),
            tools,
        )
        .await
        .unwrap()
        .with_remote_host(remote_configuration())
        .await
        .unwrap();
        let patch = serde_json::json!({
            "patchText": "*** Begin Patch\n*** Add File: published.txt\n+first\n*** Add File: published.txt/second.txt\n+second\n*** End Patch"
        });

        let failure = server
            .run_remote_execution(
                stored_operation(&server, "file_apply_patch", patch.clone()).await,
                CancellationToken::new(),
                None,
            )
            .await;
        assert_eq!(failure.kind, OutcomeKind::Failed);
        assert!(failure.side_effects_possible);
        assert_eq!(
            tokio::fs::read_to_string(root.path().join("published.txt"))
                .await
                .unwrap(),
            "first\n"
        );

        tokio::fs::remove_file(root.path().join("published.txt"))
            .await
            .unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let cancellation = server
            .run_remote_execution(
                stored_operation(&server, "file_apply_patch", patch).await,
                cancelled,
                None,
            )
            .await;
        assert_eq!(cancellation.kind, OutcomeKind::Cancelled);
        assert!(!cancellation.side_effects_possible);
        assert!(!root.path().join("published.txt").exists());
    }

    #[test]
    fn composed_catalog_is_exact_and_ordered() {
        let catalog = compose_catalog([
            file_catalog(true),
            workcell_mcp_code_graph::catalog(),
            web_catalog(2026, &WebsearchExecutionConfiguration::unconfigured()),
            workcell_mcp_shell::catalog(),
            workcell_mcp_code::catalog(),
            vec![execution_environment_tool()],
        ])
        .unwrap();
        for tool in &catalog {
            assert!(PreparedRemoteOperation::supports(tool.name.as_ref()));
            assert!(
                tool.output_schema.is_some(),
                "{} has no output schema",
                tool.name
            );
            let contract = &tool.meta.as_ref().unwrap().0["ai.workcell/contract"];
            assert!(
                contract["id"].is_string(),
                "{} has no contract id",
                tool.name
            );
            assert_eq!(contract["version"], "v1", "{} contract", tool.name);
            assert_eq!(contract["resultVersion"], "v1", "{} result", tool.name);
            assert!(
                tool.meta.as_ref().unwrap().0["ai.workcell/presentation-profile"].is_string(),
                "{} has no presentation profile",
                tool.name
            );
        }
        let names = catalog
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "file_read",
                "file_glob",
                "file_grep",
                "file_write",
                "file_edit",
                "file_apply_patch",
                "file_index",
                "code_map",
                "code_context",
                "code_refs",
                "code_impact",
                "code_expand",
                "websearch",
                "webfetch",
                "shell",
                "python_execution",
                "execution_environment",
            ]
        );
    }

    #[test]
    fn snapshot_restore_intents_disclose_created_directories_and_private_journal_effects() {
        let preview = SnapshotRestorePreview {
            restore_id: Identifier::new("restore_intent").unwrap(),
            target_snapshot_id: Identifier::new(format!("snap_{}", "0".repeat(64))).unwrap(),
            current_revision: Revision::new("current").unwrap(),
            target_revision: Revision::new("target").unwrap(),
            changes: Vec::new(),
            created_directories: vec![WorkspacePath::new("one/two").unwrap()],
        };
        let pre_restore_snapshot_id = format!("snap_{}", "1".repeat(64));
        let intents = snapshot_restore_intents(&preview, &pre_restore_snapshot_id).unwrap();
        assert_eq!(intents.len(), 4);
        assert_eq!(intents[0].display.as_str(), "file:one/two");
        assert_eq!(intents[0].access, ResourceAccess::Write);
        assert_eq!(
            intents[1].display.as_str(),
            format!("snapshot-store:pre-restore:{pre_restore_snapshot_id}:manifest-and-blobs")
        );
        assert_eq!(intents[1].access, ResourceAccess::Write);
        assert_eq!(
            intents[2].display.as_str(),
            "snapshot-store:journal:restore_intent"
        );
        assert_eq!(intents[2].access, ResourceAccess::Write);
        assert_eq!(
            intents[3].display.as_str(),
            "snapshot-store:acknowledged-restore-journals"
        );
        assert_eq!(intents[3].access, ResourceAccess::Delete);
    }

    #[tokio::test]
    async fn websearch_intent_matches_the_exact_embedded_permission_query() {
        let root = tempfile::tempdir().unwrap();
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[ToolGroup::Web],
            ServerBehavior::default(),
            test_tools(),
        )
        .await
        .unwrap();
        let (operation, intent) = server
            .prepare_operation(
                "websearch",
                serde_json::json!({"query":"  exact permission query  "}),
            )
            .await
            .unwrap();
        let PreparedRemoteOperation::Websearch(prepared) = operation else {
            panic!("expected prepared websearch")
        };

        assert_eq!(intent.resources.len(), 2);
        assert_eq!(intent.resources[0].display.as_str(), "web:search");
        assert_eq!(intent.resources[0].access, ResourceAccess::Connect);
        assert_eq!(
            intent.resources[1].display.as_str(),
            prepared.permission_query()
        );
        assert_eq!(
            intent.resources[1].display.as_str(),
            "exact permission query"
        );
        assert_eq!(intent.resources[1].access, ResourceAccess::Search);
        assert_eq!(
            intent.resources[1].resource_id,
            resource_id("query/search", prepared.permission_query()).unwrap()
        );
    }

    #[test]
    fn calendar_conversion_covers_year_boundaries() {
        assert_eq!(year_from_unix_days(0), 1970);
        assert_eq!(year_from_unix_days(19_723), 2024);
    }

    #[tokio::test]
    async fn execution_environment_tool_follows_disclosure_switch() {
        let disabled = WorkcellServer::configured(
            None,
            &[],
            ServerBehavior {
                expose_execution_environment: false,
                modern_only: false,
            },
            test_tools(),
        )
        .await
        .unwrap();
        assert!(disabled.catalog().is_empty());
        assert!(
            disabled
                .dispatch(
                    EXECUTION_ENVIRONMENT_TOOL,
                    serde_json::json!({}),
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );

        let enabled = WorkcellServer::configured(
            None,
            &[],
            ServerBehavior {
                expose_execution_environment: true,
                modern_only: false,
            },
            test_tools(),
        )
        .await
        .unwrap();
        assert_eq!(enabled.catalog()[0].name, EXECUTION_ENVIRONMENT_TOOL);
    }

    #[tokio::test]
    async fn each_server_freezes_one_catalog_with_a_deterministic_revision() {
        let first = WorkcellServer::configured(None, &[], ServerBehavior::default(), test_tools())
            .await
            .unwrap();
        let second = WorkcellServer::configured(None, &[], ServerBehavior::default(), test_tools())
            .await
            .unwrap();

        assert_eq!(first.catalog_revision(), second.catalog_revision());
        assert_eq!(first.catalog_revision(), &first.tool_manifest().revision);
        assert_eq!(first.tool_manifest().version, "v2");
        assert_eq!(
            Arc::as_ptr(&first.catalog),
            Arc::as_ptr(&first.clone().catalog)
        );
        assert_eq!(first.instance_id, second.instance_id);
    }

    #[tokio::test]
    async fn remote_descriptor_claims_exact_typed_preparation() {
        let (_root, remote) = test_remote_state().await;
        assert_eq!(
            remote.descriptor.workspace_generation.as_str(),
            "generation"
        );
        assert!(
            remote
                .descriptor
                .capabilities
                .operations
                .as_ref()
                .unwrap()
                .exact_preparation
        );
        assert_eq!(
            remote
                .descriptor
                .capabilities
                .scm
                .as_ref()
                .unwrap()
                .limits
                .max_paths,
            u32::try_from(workcell_host_contract::MAX_SCM_PATHS).unwrap()
        );
    }

    #[test]
    fn durable_workspace_binding_changes_with_the_configured_generation() {
        let first = remote_configuration();
        let mut second = first.clone();
        second.workspace_generation = Identifier::new("other-generation").unwrap();

        assert_ne!(
            durable_workspace_binding(&first).unwrap(),
            durable_workspace_binding(&second).unwrap()
        );
    }

    #[tokio::test]
    async fn remote_host_omits_scm_when_the_bounded_git_probe_fails() {
        let root = tempfile::tempdir().unwrap();
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[],
            ServerBehavior::default(),
            test_tools(),
        )
        .await
        .unwrap()
        .with_remote_host_git(remote_configuration(), root.path().join("missing-git"))
        .await
        .unwrap();
        let capabilities = &server.remote_host.as_ref().unwrap().descriptor.capabilities;

        assert!(server.scm.is_none());
        assert!(capabilities.scm.is_none());
        assert!(!capabilities.control_plane);
        assert!(
            capabilities
                .control_plane_missing
                .iter()
                .any(|name| name.as_str() == "scm")
        );
    }

    #[tokio::test]
    async fn remote_execution_consumes_exact_stored_values_across_host_owned_groups() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(root.path().join("scope"))
            .await
            .unwrap();
        tokio::fs::write(
            root.path().join("scope/lib.rs"),
            "pub fn exact_scope() {}\n",
        )
        .await
        .unwrap();
        let mut tools = test_tools();
        tools.allow_write = true;
        tools.shell_policy = ShellPermissionPolicy::yolo();
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[
                ToolGroup::Files,
                ToolGroup::CodeGraph,
                ToolGroup::Web,
                ToolGroup::Shell,
                ToolGroup::Transfer,
            ],
            ServerBehavior {
                expose_execution_environment: true,
                modern_only: true,
            },
            tools,
        )
        .await
        .unwrap()
        .with_remote_host(remote_configuration())
        .await
        .unwrap();

        let operation = stored_operation(
            &server,
            "file_write",
            serde_json::json!({"filePath":"exact.txt","content":"stored"}),
        )
        .await;
        let result = server
            .execute_prepared_operation(operation, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["applied"], true);
        assert_eq!(
            tokio::fs::read_to_string(root.path().join("exact.txt"))
                .await
                .unwrap(),
            "stored"
        );

        let (operation, intent) = server
            .prepare_operation(
                "shell",
                serde_json::json!({"command":"pwd","workdir":"scope"}),
            )
            .await
            .unwrap();
        assert!(intent.mutating);
        assert_eq!(intent.resources.len(), 3);
        assert_eq!(intent.resources[0].display.as_str(), "scope");
        assert_eq!(intent.resources[0].access, ResourceAccess::Traverse);
        assert_eq!(intent.resources[1].display.as_str(), "pwd");
        assert_eq!(intent.resources[1].access, ResourceAccess::Execute);
        assert_eq!(intent.resources[2].access, ResourceAccess::Inspect);
        let result = server
            .execute_prepared_operation(operation, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.unwrap()["relativeWorkdir"],
            "scope"
        );

        let operation = stored_operation(
            &server,
            "webfetch",
            serde_json::json!({"url":"https://example.com/exact"}),
        )
        .await;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let result = server
            .execute_prepared_operation(operation, cancelled, None)
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));

        let operation = stored_operation(
            &server,
            "code_map",
            serde_json::json!({"path":"scope","limit":1}),
        )
        .await;
        let result = server
            .execute_prepared_operation(operation, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["path"], "scope");

        let operation =
            stored_operation(&server, EXECUTION_ENVIRONMENT_TOOL, serde_json::json!({})).await;
        let result = server
            .execute_prepared_operation(operation, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.unwrap()["toolGroups"]["files"],
            true
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_preconditions_reject_stale_content_scopes_workdirs_and_configuration() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("mutable.txt"), "before")
            .await
            .unwrap();
        tokio::fs::create_dir(root.path().join("graph"))
            .await
            .unwrap();
        tokio::fs::write(root.path().join("graph/lib.rs"), "fn before() {}\n")
            .await
            .unwrap();
        tokio::fs::create_dir(root.path().join("work-a"))
            .await
            .unwrap();
        tokio::fs::create_dir(root.path().join("work-b"))
            .await
            .unwrap();
        symlink("work-a", root.path().join("work")).unwrap();
        let mut tools = test_tools();
        tools.allow_write = true;
        tools.shell_policy = ShellPermissionPolicy::yolo();
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[
                ToolGroup::Files,
                ToolGroup::CodeGraph,
                ToolGroup::Web,
                ToolGroup::Shell,
                ToolGroup::Transfer,
            ],
            ServerBehavior::default(),
            tools,
        )
        .await
        .unwrap()
        .with_remote_host(remote_configuration())
        .await
        .unwrap();

        let file = stored_operation(
            &server,
            "file_write",
            serde_json::json!({"filePath":"mutable.txt","content":"prepared"}),
        )
        .await;
        let graph =
            stored_operation(&server, "code_map", serde_json::json!({"path":"graph"})).await;
        let shell = stored_operation(
            &server,
            "shell",
            serde_json::json!({"command":"pwd","workdir":"work"}),
        )
        .await;
        let web = stored_operation(
            &server,
            "websearch",
            serde_json::json!({"query":"stored query"}),
        )
        .await;

        tokio::fs::write(root.path().join("mutable.txt"), "changed")
            .await
            .unwrap();
        tokio::fs::rename(root.path().join("graph"), root.path().join("graph-moved"))
            .await
            .unwrap();
        tokio::fs::remove_file(root.path().join("work"))
            .await
            .unwrap();
        symlink("work-b", root.path().join("work")).unwrap();
        server.web.as_ref().unwrap().clear_configuration();

        for operation in [file, graph, shell, web] {
            let result = server
                .execute_prepared_operation(operation, CancellationToken::new(), None)
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(true));
        }
        assert_eq!(
            tokio::fs::read_to_string(root.path().join("mutable.txt"))
                .await
                .unwrap(),
            "changed"
        );
    }

    #[tokio::test]
    async fn remote_python_execution_consumes_the_stored_snippet() {
        let Some(worker) = test_worker() else {
            eprintln!("skipping: no monty worker; run make code-worker");
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let mut tools = test_tools();
        tools.code = CodeConfiguration {
            worker: WorkerSource::Path(&worker),
            type_check: true,
        };
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[ToolGroup::PythonExecution],
            ServerBehavior::default(),
            tools,
        )
        .await
        .unwrap()
        .with_remote_host(remote_configuration())
        .await
        .unwrap();
        let (operation, intent) = server
            .prepare_operation("python_execution", serde_json::json!({"code":"40 + 2"}))
            .await
            .unwrap();
        assert!(!intent.mutating);
        assert_eq!(intent.kind, OperationKind::Execute);
        assert_eq!(intent.resources.len(), 1);
        assert_eq!(
            intent.resources[0].display.as_str(),
            ISOLATED_PYTHON_RESOURCE
        );
        assert_eq!(intent.resources[0].access, ResourceAccess::Execute);
        let result = server
            .execute_prepared_operation(operation, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["result"], 42);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_the_response_waiter_does_not_drop_the_owned_operation() {
        let (_root, remote) = test_remote_state().await;
        let (preparation_id, invocation_id, lease) = running_operation(&remote);
        let guard = RemoteExecutionGuard::new(
            remote.clone(),
            preparation_id.clone(),
            invocation_id.clone(),
            lease,
        );
        let (release, released) = tokio::sync::oneshot::channel();
        let (finished, completion) = tokio::sync::oneshot::channel();
        let response_waiter = tokio::spawn(async move {
            let _ = released.await;
            guard.finish(cancelled_outcome(None, false));
            let _ = finished.send(());
        });
        drop(response_waiter);
        let _ = release.send(());
        completion.await.unwrap();

        assert_eq!(
            remote
                .status(&preparation_id, Some(&invocation_id), &remote.binding,)
                .unwrap()
                .state,
            workcell_host_contract::OperationState::Cancelled
        );
    }

    #[tokio::test]
    async fn panic_and_abort_both_terminalize_running_records() {
        for abort in [false, true] {
            let (_root, remote) = test_remote_state().await;
            let (preparation_id, invocation_id, lease) = running_operation(&remote);
            let guard = RemoteExecutionGuard::new(
                remote.clone(),
                preparation_id.clone(),
                invocation_id.clone(),
                lease,
            );
            let task = tokio::spawn(async move {
                let _guard = guard;
                if abort {
                    std::future::pending::<()>().await;
                } else {
                    panic!("operation panic");
                }
            });
            if abort {
                task.abort();
            }
            let _ = task.await;

            assert_eq!(
                remote
                    .status(&preparation_id, Some(&invocation_id), &remote.binding,)
                    .unwrap()
                    .state,
                workcell_host_contract::OperationState::Indeterminate
            );
        }
    }

    #[test]
    fn argument_and_resource_ids_are_stable_and_canonical() {
        let first = serde_json::json!({"outer":{"b":2,"a":1},"array":[2,1]});
        let reordered = serde_json::json!({"array":[2,1],"outer":{"a":1,"b":2}});
        assert_eq!(
            argument_digest(&first).unwrap(),
            argument_digest(&reordered).unwrap()
        );
        assert_eq!(
            argument_digest(&first).unwrap().as_str(),
            "sha256:8aa7f3046e840ccb2330ee5728f0429bdda6ec56347da9c5eab069a415d1bf7e"
        );

        let display = format!("https://example.invalid/{}\npath", "x".repeat(2_048));
        let first = resource_intent(&display, ResourceAccess::Connect).unwrap();
        let second = resource_intent(&display, ResourceAccess::Connect).unwrap();
        assert_eq!(first.resource_id, second.resource_id);
        assert_eq!(first.display.as_str(), display);
        assert_ne!(first.resource_id.as_str(), first.display.as_str());
        assert_eq!(
            first.resource_id.as_str(),
            "sha256:6e2c67f7e841af2d05f2a0f4162f43132cbe8e0228f8b64ba7adac2a95b8a433"
        );
    }

    #[test]
    fn symbolic_json_rpc_error_data_uses_the_code_field_fixture() {
        assert_eq!(
            serde_json::to_value(remote_error(
                crate::remote_host::RemoteOperationError::InvalidRequest,
            ))
            .unwrap(),
            serde_json::json!({
                "code": -32602,
                "message": "remote operation request is invalid",
                "data": {"code": "invalid_request"},
            })
        );
    }

    async fn test_remote_state() -> (TempDir, RemoteHostState) {
        let root = tempfile::tempdir().unwrap();
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[],
            ServerBehavior::default(),
            test_tools(),
        )
        .await
        .unwrap()
        .with_remote_host(
            RemoteHostConfiguration::new(
                "server".into(),
                "workspace".into(),
                "generation".into(),
                "project".into(),
                "principal".into(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        (root, server.remote_host.unwrap())
    }

    async fn stored_operation(
        server: &WorkcellServer,
        name: &str,
        arguments: Value,
    ) -> PreparedRemoteOperation {
        let remote = server.remote_host.as_ref().unwrap();
        let prepared_bytes = serde_json::to_vec(&arguments).unwrap().len();
        let argument_digest = argument_digest(&arguments).unwrap();
        let (operation, intent) = server.prepare_operation(name, arguments).await.unwrap();
        let reservation = remote.reserve_preparation(prepared_bytes).unwrap();
        let response = remote
            .prepare_reserved(
                reservation,
                operation,
                OperationBinding {
                    host: remote.binding.clone(),
                    contract: ContractBinding {
                        id: Identifier::new("test.v1").unwrap(),
                        version: Identifier::new("v1").unwrap(),
                        result_version: Identifier::new("v1").unwrap(),
                    },
                    argument_digest,
                },
                intent,
            )
            .unwrap();
        let request = workcell_host_contract::ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: response.preparation_id,
            invocation_id: Identifier::new(format!("invocation-{}", Uuid::new_v4())).unwrap(),
            host: remote.binding.clone(),
        };
        let BeginExecution::Start { operation, .. } =
            remote.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("stored operation did not start")
        };
        *operation
    }

    fn remote_configuration() -> RemoteHostConfiguration {
        RemoteHostConfiguration::new(
            "server".into(),
            "workspace".into(),
            "generation".into(),
            "project".into(),
            "principal".into(),
        )
        .unwrap()
    }

    fn test_worker() -> Option<PathBuf> {
        let configured = std::env::var_os("WORKCELL_MCP_CODE_WORKER").map(PathBuf::from);
        let built = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/code-worker/bin")
            .join(WORKER_FILE_NAME);
        configured
            .into_iter()
            .chain([built])
            .find(|path| path.is_file())
    }

    fn running_operation(
        remote: &RemoteHostState,
    ) -> (Identifier, Identifier, crate::remote_host::ExecutionLease) {
        let contract = ContractBinding {
            id: Identifier::new("test.v1").unwrap(),
            version: Identifier::new("v1").unwrap(),
            result_version: Identifier::new("v1").unwrap(),
        };
        let binding = OperationBinding {
            host: remote.binding.clone(),
            contract,
            argument_digest: Revision::new("sha256:arguments").unwrap(),
        };
        let reservation = remote.reserve_preparation(1).unwrap();
        let prepared = remote
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::Test(Vec::new()),
                binding,
                OperationIntent {
                    kind: OperationKind::Execute,
                    mutating: false,
                    resources: Vec::new(),
                },
            )
            .unwrap();
        let invocation_id = Identifier::new("invocation").unwrap();
        let request = workcell_host_contract::ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id.clone(),
            invocation_id: invocation_id.clone(),
            host: remote.binding.clone(),
        };
        let BeginExecution::Start { lease, .. } =
            remote.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("operation did not start")
        };
        (prepared.preparation_id, invocation_id, lease)
    }
}
