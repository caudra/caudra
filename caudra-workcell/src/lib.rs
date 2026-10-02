#![forbid(unsafe_code)]

mod changes;
pub mod editor_adapter;
mod native_redirect;
mod pattern_analysis;
mod read_only_shell;
mod remote;
mod shell_record_scope;
mod transfer;
mod transfer_authorization;
mod transfer_inventory;
mod transfer_session;

pub use changes::{ChangeInventory, LocalChangeStore, LocalChangeStores, PreparedStoreCleanup};
pub use pattern_analysis::{
    BashContextAssumptions, BashContextIssue, BashOperatorKind, BashSpan,
    COMMAND_OBSERVATION_ATTRIBUTE, MAX_PATTERN_DIAGNOSTIC_DETAILS, PatternCallAnalysis,
    PatternCallDiagnostics, PatternCommandOmission, PatternObligationCount, PatternObligationKind,
    PatternOmissionReason, PatternSourceObligation, PatternSourceObligations,
    analyze_pattern_calls,
};
pub use remote::{
    NamedBearerCredential, PendingRemoteOperation, RemoteConnectionStatus, RemoteEvent,
    RemotePreparedToolCall, RemoteToolResultEnvelope, RemoteWorkcellClient, RemoteWorkcellError,
};
use remote::{RemoteToolExecutionError, STALE_RESOURCE_CODE};
pub use transfer::LocalTransferPublisher;
pub use transfer_authorization::NativeTransferAuthorization;
pub use transfer_inventory::{
    ReviewedTransferHost, RootedTransferInventory, reviewed_workspace_transfer,
};
pub use transfer_session::{
    TransferReport, TransferSession, TransferSessionHost, TransferValidity,
};
pub use workcell::host_contract;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caudra_agent::herdr::PANE_ENVIRONMENT;
use caudra_agent::patch;
use caudra_agent::permissions::pattern_recognition::ObservationProvenance;
use caudra_agent::permissions::{
    COMMAND_OBSERVATION_BINDING_ATTRIBUTE, CONFINED_READ_ATTRIBUTE, CONFINED_READ_VALUE,
    OPACITY_ATTRIBUTE, PermissionAuthorityProfile, PermissionResource, PermissionResourceAccess,
    PermissionResourceKind, PermissionRisk, RemotePermissionIdentity, ShellOpacity,
    filesystem_permission_resource, prepared_command_binding, shell_permission_scope,
};
use caudra_agent::tools::{
    BoxFuture, DEADLINE_EXCEEDED, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult,
    LockKey, PYTHON_EXECUTION_TOOL_NAME, ParseError, PermissionIntent, PermissionScopes,
    PlanModeAccess, RegistryError, SHELL_TOOL_NAME, Tool, ToolAudience, ToolContext, ToolEffect,
    ToolError, ToolExecResult, ToolFailure, ToolInvocation, ToolLive, ToolRegistry, ToolSource,
    expand_tilde, stale_read_message,
};
use caudra_agent::{
    AgentEvent, CodeGraphRow, CodeGraphSource, EnvironmentCommand, EnvironmentFact, GrepFileEntry,
    GrepMatchGroup, INDEX_TRUNCATED, IndexDirectoryEntry as AgentIndexDirectoryEntry,
    IndexDirectoryEntryKind as AgentIndexDirectoryEntryKind, IndexLine as AgentIndexLine,
    IndexLineSemantic as AgentIndexLineSemantic, IndexOutput as AgentIndexOutput,
    IndexSourceRange as AgentIndexSourceRange, PatchedFile, SearchCap, SharedBuf,
    ShellFilterInfo as AgentShellFilterInfo, ShellOutput as AgentShellOutput, SnapshotLine,
    TextOutput, ToolInput, ToolOutput,
};
use caudra_config::ShellNativeRedirect;
use caudra_storage::StateDir;
use caudra_storage::permission_state::{
    BROWSE_DIRECT, BROWSE_RECURSION_ATTRIBUTE, BROWSE_RECURSIVE,
};
use caudra_workspace::{
    OperationError, OperationProgressKind, OperationState, OperationStatus, PreparedToolCall,
    RecordScope, RecordedPath, ToolPrepareRequest, UNREVEALED_ROOT, WorkspaceChangeService,
    WorkspaceError,
};
use futures_lite::future;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use workcell::code::bundled_worker_available;
use workcell::code::{CodeConfiguration, CodeExecution, CodeInput, Outcome, WorkerSource};
use workcell::code_graph::{
    CodeContextInput, CodeExpandInput, CodeGraphError, CodeGraphLimits, CodeGraphToolGroup,
    CodeImpactInput, CodeMapInput, CodeRefsInput, GraphProgress, GraphProgressSink,
    ModelText as CodeGraphModelText, RankedSymbol, ReachedSymbol, SelectorRefusal, SymbolRef,
    crawl_filesystem_limits, fit,
};
use workcell::environment::{
    ExecutionEnvironmentError, ExecutionEnvironmentResult, ToolGroupDisclosure,
};
use workcell::files::{
    FileApplyPatchInput, FileApplyPatchOutput, FileDiff, FileEditInput, FileEditOutput,
    FileGlobInput, FileGlobOutput, FileGrepInput, FileGrepOutput, FileReadInput, FileReadOutput,
    FileResource, FileResourceAccess, FileToolGroup, FileWriteInput, FileWriteOutput,
    FilesystemError, IndexDirectoryEntryKind, IndexExecutionConfiguration, IndexInput, IndexLimits,
    IndexLineSemantic, IndexOutput as WorkcellIndexOutput, ModelText, PreparedFilePatch,
    PreparedFileRead,
};
use workcell::output_filter::RowRenderer;
use workcell::shell::bash::{BashCommandContexts, BashProgram};
use workcell::shell::{
    MAX_TIMEOUT_MS as SHELL_MAX_TIMEOUT_MS, PreparedShell, ShellExecution,
    ShellFilterInfo as WorkcellShellFilterInfo, ShellInput, ShellOutput as WorkcellShellOutput,
    ShellPreparationError, ShellProgressChunk, ShellProgressSink, ShellStream, ShellToolGroup,
};
use workcell::web::{
    PreparedWebfetch, PreparedWebsearch, ProxyConfiguration, WebExecution, WebToolGroup,
    WebfetchError, WebfetchInput, WebfetchOutput, WebsearchExecutionConfiguration, WebsearchInput,
    WebsearchOutput,
};
use workcell::{CodeToolGroup, ExecutionEnvironment};
use workcell::{OwnedToolSpec, ToolSpec};

pub const OWNER: &str = "workcell";
/// Shared with the tests so a wording change cannot silently pass an assertion.
pub const MISSING_READ_TARGET: &str = "No such file or directory";
/// How a shell result's `relative_workdir` names the project directory itself.
pub const CURRENT_WORKDIR: &str = ".";
pub const NATIVE_TOOL_NAMES: &[&str] = &[
    "file_read",
    "file_glob",
    "file_grep",
    "file_write",
    "file_edit",
    "file_apply_patch",
    "file_index",
    "websearch",
    "webfetch",
    "shell",
    "python_execution",
    "code_map",
    "code_context",
    "code_refs",
    "code_impact",
    "code_expand",
    "execution_environment",
];
const CODE_WORKER_UNAVAILABLE: &str =
    "Workcell python_execution is unavailable: no code worker path was supplied";
const SHELL_CANCELLED: &str = "Shell execution cancelled";
const CODE_CANCELLED: &str = "Code execution cancelled";
/// Every way a snippet can end, so a remote result's outcome is read by the
/// names Workcell itself serializes rather than by a copy of them.
const CODE_OUTCOMES: [Outcome; 5] = [
    Outcome::Completed,
    Outcome::Exception,
    Outcome::Rejected,
    Outcome::Limited,
    Outcome::Unavailable,
];
const CODE_OUTCOME_FIELD: &str = "outcome";
const CODE_TIMED_OUT_FIELD: &str = "timedOut";
const PROGRESS_MAX_BYTES: usize = 64 * 1024;
const PROGRESS_TRUNCATED: &str = "[earlier output truncated]\n";
const BYTES_PER_MIB: usize = 1024 * 1024;
const NORMALIZED_COMMAND_ATTRIBUTE: &str = "normalized_command";
const REMOTE_PROGRESS_GAP: &str = "Remote progress is partial; earlier output was not retained. The final result is authoritative.";
const REMOTE_INDETERMINATE: &str =
    "Remote Workcell outcome is indeterminate; do not retry automatically";
const REMOTE_FORGOTTEN: &str =
    "Remote Workcell forgot the operation outcome; do not retry automatically";
const REMOTE_DISPLAY_MAX_CHARS: usize = 512;
const REMOTE_POLL_INITIAL: Duration = Duration::from_millis(100);
const REMOTE_POLL_MAX: Duration = Duration::from_secs(2);
const REMOTE_EXECUTION_TIMEOUT: Duration = Duration::from_secs(600);
/// Headroom for cleanup and result delivery once a command has used the whole
/// deadline Workcell allows it. It buys the client no extra execution time: the
/// server stops the process on its own timer, and waiting slightly longer is
/// what turns a completed run into a result instead of a cancellation.
const SHELL_COMPLETION_ALLOWANCE_MS: u64 = 30_000;
const SHELL_EXECUTION_TIMEOUT: Duration =
    Duration::from_millis(SHELL_MAX_TIMEOUT_MS + SHELL_COMPLETION_ALLOWANCE_MS);
const REMOTE_RECONCILE_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_RECONCILE_MAX_POLLS: usize = 4;
const REMOTE_DEADLINE_BEFORE_DISPATCH: &str =
    "Remote Workcell execution deadline expired before dispatch";
const REMOTE_CONTEXT_UNAVAILABLE: &str = "Remote Workcell workspace context is unavailable";
/// A preparation lives on the server's own timer, so a call that waits behind
/// review can outlive it. Renewing before dispatch keeps a legitimate call
/// alive; the renewed intent still has to be the one that was reviewed.
const REMOTE_PREPARATION_RENEWAL: Duration = Duration::from_secs(15);
const REMOTE_PREPARATION_LAPSED: &str =
    "Remote Workcell preparation expired before dispatch and could not be renewed";
const REMOTE_PREPARATION_CHANGED: &str = "Remote Workcell preparation expired before dispatch and the renewed request no longer matches the reviewed intent";
/// What a remote host promises about the shell it runs a line in. A prepared
/// call reporting anything else is not one Caudra can review.
const REMOTE_SHELL_ASSUMPTIONS: BashContextAssumptions = BashContextAssumptions {
    startup_preserves_cwd: true,
    no_aliases_functions_or_command_not_found_hook: true,
    no_traps: true,
    default_shell_options: true,
    standard_builtins: true,
    directory_variables_are_standard: true,
    cdpath_empty: true,
    lastpipe_disabled: true,
    logical_pwd_matches_initial: true,
};
/// Caudra owns authorization, so Workcell always hands over the mutation
/// tools and every write still passes through the permission layer first.
/// Withholding them here would hide tools the user is allowed to approve.
const ALLOW_WRITE: bool = true;
/// Source icons are a second fetch per result for decoration the TUI does not
/// render.
const SOURCE_ICONS_ENABLED: bool = false;
/// Read in this order, first value wins, matching what every other HTTP client
/// on the machine already does.
const PROXY_ALL_VARS: &[&str] = &["ALL_PROXY", "all_proxy"];
const PROXY_HTTP_VARS: &[&str] = &["HTTP_PROXY", "http_proxy"];
const PROXY_HTTPS_VARS: &[&str] = &["HTTPS_PROXY", "https_proxy"];
const PROXY_BYPASS_VARS: &[&str] = &["NO_PROXY", "no_proxy"];

/// The labels an environment result is read by, and the few host answers the
/// rendering has to recognise rather than repeat.
const ENVIRONMENT_SEPARATOR: &str = " \u{b7} ";
const ENVIRONMENT_RUNTIME_LABEL: &str = "runtime";
const ENVIRONMENT_CONTAINER_LABEL: &str = "container";
const ENVIRONMENT_PACKAGES_LABEL: &str = "packages";
const ENVIRONMENT_WORKSPACE_LABEL: &str = "workspace";
const ENVIRONMENT_GROUPS_LABEL: &str = "groups";
const ENVIRONMENT_INHERITANCE_LABEL: &str = "env";
const ENVIRONMENT_LIST_SEPARATOR: &str = ", ";
const ENVIRONMENT_NONE: &str = "none";
/// A platform with no sudo to test says so; saying it on every Windows result
/// would spend a column on a question that cannot be asked there.
const SUDO_NOT_APPLICABLE: &str = "not-applicable";
const GIT_REPOSITORY_YES: &str = "yes";
const GIT_REPOSITORY_NO: &str = "no";
/// A diff on its own reads as a change that landed, so a result that changed
/// nothing says so where the model cannot miss it.
const NOT_APPLIED: &str = "[not applied: nothing was written]";

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("create Workcell Tokio runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("initialize Workcell filesystem tools: {0}")]
    Files(String),
    #[error("initialize Workcell shell tool: {0}")]
    Shell(String),
}

#[derive(Clone)]
struct ProjectGroups {
    files: FileToolGroup,
    shell: ShellToolGroup,
    code_graph: Arc<CodeGraphToolGroup>,
    environment: Arc<ExecutionEnvironment>,
}

/// The code graph reads through its own filesystem group.
///
/// It cannot share the one the file tools use: that one is write-enabled and
/// carries no traversal bound, and `from_files` clamps the crawl to whatever
/// bound its group was built with. A second read-only group built from the
/// crawl's own limits is what keeps a whole-tree map from silently stopping
/// early.
async fn code_graph_group(cwd: &Path) -> Result<Arc<CodeGraphToolGroup>, String> {
    let limits = CodeGraphLimits::default();
    let files = FileToolGroup::new_unconfined(cwd, false, Some(crawl_filesystem_limits(&limits)))
        .await
        .map_err(|error| error.to_string())?;
    Ok(Arc::new(CodeGraphToolGroup::from_files(
        Arc::new(files),
        limits,
    )))
}

async fn shell_group(cwd: &Path) -> Result<ShellToolGroup, String> {
    ShellToolGroup::new_unconfined(cwd)
        .await
        .map(|group| group.with_inherited_environment(PANE_ENVIRONMENT))
        .map_err(|error| error.to_string())
}

struct HostInner {
    runtime: Runtime,
    projects: tokio::sync::Mutex<HashMap<PathBuf, ProjectGroups>>,
    web: WebToolGroup,
    code: Option<Arc<CodeToolGroup>>,
}

impl HostInner {
    fn specs(&self, include_unavailable_code: bool) -> Vec<ToolSpec> {
        let mut specs = workcell::files::specs(ALLOW_WRITE);
        let year = jiff::Timestamp::now()
            .strftime("%Y")
            .to_string()
            .parse()
            .unwrap_or(2026);
        specs.extend(workcell::web::specs(
            year,
            &self.web.snapshot().configuration,
        ));
        specs.extend(workcell::shell::specs());
        specs.extend(workcell::code_graph::specs());
        if self.code.is_some() || include_unavailable_code {
            specs.extend(workcell::code::specs());
        }
        specs.push(workcell::environment::spec());
        specs
    }

    async fn project_groups(&self, cwd: PathBuf) -> Result<ProjectGroups, String> {
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let mut projects = self.projects.lock().await;
        if let Some(groups) = projects.get(&cwd) {
            return Ok(groups.clone());
        }
        let files = FileToolGroup::new_unconfined(&cwd, ALLOW_WRITE, None)
            .await
            .map_err(|error| error.to_string())?;
        let shell = shell_group(&cwd).await?;
        let code_graph = code_graph_group(&cwd).await?;
        let environment = Arc::new(ExecutionEnvironment::new(Some(&cwd)).await);
        let groups = ProjectGroups {
            files,
            shell,
            code_graph,
            environment,
        };
        projects.insert(cwd, groups.clone());
        Ok(groups)
    }

    /// Fails only for reasons of its own: the caller's deadline, or a runtime
    /// task that never returned. A cancelled caller still gets the operation's
    /// result, because the operation is what knows how it stopped.
    async fn run<T, F, Fut>(&self, ctx: &ToolContext, operation: F) -> Result<T, ToolError>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let deadline = ctx
            .deadline
            .remaining()
            .map_err(|message| ToolError::new(ToolFailure::Timeout, message))?;
        let cancellation = CancellationToken::new();
        if ctx.cancel.is_cancelled() {
            cancellation.cancel();
        }
        let operation_cancellation = cancellation.clone();
        let operation = operation(operation_cancellation.clone());
        let mut task = Box::pin(self.runtime.spawn(async move {
            tokio::pin!(operation);
            let Some(deadline) = deadline else {
                return Some(operation.await);
            };
            tokio::select! {
                output = &mut operation => Some(output),
                () = tokio::time::sleep(deadline) => {
                    operation_cancellation.cancel();
                    let _ = operation.await;
                    None
                }
            }
        }));
        enum Completion<T> {
            Done(Result<T, tokio::task::JoinError>),
            Cancelled,
        }
        let completion = future::race(async { Completion::Done(task.as_mut().await) }, async {
            ctx.cancel.cancelled().await;
            cancellation.cancel();
            Completion::Cancelled
        })
        .await;
        let result = match completion {
            Completion::Done(result) => result,
            Completion::Cancelled => task.await,
        };
        match result {
            Ok(Some(output)) => Ok(output),
            Ok(None) => Err(ToolError::new(ToolFailure::Timeout, DEADLINE_EXCEEDED)),
            Err(error) => Err(ToolError::new(
                ToolFailure::Other,
                format!("Workcell runtime task failed: {error}"),
            )),
        }
    }
}

impl Drop for HostInner {
    fn drop(&mut self) {
        if let Some(code) = &self.code {
            self.runtime.block_on(code.shutdown());
        }
    }
}

pub struct WorkcellHost {
    inner: Arc<HostInner>,
    warnings: Vec<String>,
    reserve_code: bool,
}

impl WorkcellHost {
    pub fn new(
        project_cwd: impl AsRef<Path>,
        worker_path: Option<&Path>,
    ) -> Result<Self, HostError> {
        Self::with_worker_source(project_cwd, worker_path.map(WorkerSource::Path))
    }

    fn with_worker_source(
        project_cwd: impl AsRef<Path>,
        worker_source: Option<WorkerSource<'_>>,
    ) -> Result<Self, HostError> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name("caudra-workcell")
            .build()
            .map_err(HostError::Runtime)?;
        let project_cwd = std::fs::canonicalize(project_cwd.as_ref())
            .unwrap_or_else(|_| project_cwd.as_ref().to_path_buf());
        let (files, shell, code_graph, environment, code_result) = runtime.block_on(async {
            let files = FileToolGroup::new_unconfined(&project_cwd, ALLOW_WRITE, None).await;
            let shell = shell_group(&project_cwd).await;
            let code_graph = code_graph_group(&project_cwd).await;
            let environment = ExecutionEnvironment::new(Some(&project_cwd)).await;
            let code = if let Some(worker) = worker_source {
                Some(
                    CodeToolGroup::new(CodeConfiguration {
                        worker,
                        type_check: true,
                    })
                    .await,
                )
            } else {
                None
            };
            (files, shell, code_graph, environment, code)
        });
        let files = files.map_err(|error| HostError::Files(error.to_string()))?;
        let shell = shell.map_err(HostError::Shell)?;
        let code_graph = code_graph.map_err(HostError::Files)?;
        let mut warnings = Vec::new();
        let code = match code_result {
            Some(Ok(code)) => Some(Arc::new(code)),
            Some(Err(error)) => {
                warnings.push(format!(
                    "Workcell python_execution is unavailable: code worker initialization failed: {error}"
                ));
                None
            }
            None => {
                warnings.push(CODE_WORKER_UNAVAILABLE.to_owned());
                None
            }
        };
        let (proxy, proxy_warning) = ambient_proxy();
        warnings.extend(proxy_warning);
        let projects = HashMap::from([(
            project_cwd,
            ProjectGroups {
                files,
                shell,
                code_graph,
                environment: Arc::new(environment),
            },
        )]);
        Ok(Self {
            inner: Arc::new(HostInner {
                runtime,
                projects: tokio::sync::Mutex::new(projects),
                web: WebToolGroup::production_with_proxy(
                    WebsearchExecutionConfiguration::default(),
                    SOURCE_ICONS_ENABLED,
                    &proxy,
                ),
                code,
            }),
            warnings,
            reserve_code: false,
        })
    }

    pub fn new_production(
        project_cwd: impl AsRef<Path>,
        configured_worker: Option<&Path>,
    ) -> Result<Self, HostError> {
        if let Some(worker) = configured_worker {
            let mut host = Self::new(project_cwd, Some(worker))?;
            host.reserve_code = true;
            return Ok(host);
        }
        match caudra_storage::paths::cache_dir() {
            Ok(cache_root) => {
                let mut host = Self::with_worker_source(
                    project_cwd,
                    Some(WorkerSource::Bundled {
                        cache_root: &cache_root,
                    }),
                )?;
                host.reserve_code = true;
                Ok(host)
            }
            Err(error) => {
                let mut host = Self::new(project_cwd, None)?;
                host.reserve_code = true;
                host.warnings.clear();
                host.warnings.push(format!(
                    "Workcell python_execution cache is unavailable: {error}"
                ));
                Ok(host)
            }
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The change records of the session directory `cwd`, kept beneath
    /// `state` and bound to the file tools of `cwd`, so a revert waits on the
    /// same lock as their writes. The store opens on the first call that
    /// needs it.
    pub fn change_service(&self, cwd: &Path, state: &StateDir) -> Arc<dyn WorkspaceChangeService> {
        changes::bound(Arc::clone(&self.inner), cwd, state)
    }

    pub fn register(&self, registry: &ToolRegistry) -> Result<(), RegistryError> {
        registry.register_many_audited(self.entries(false))
    }

    pub fn register_documented_tools(&self, registry: &ToolRegistry) -> Result<(), RegistryError> {
        registry.register_many_audited(self.entries(true))
    }

    fn entries(
        &self,
        include_unavailable_code: bool,
    ) -> Vec<(Arc<dyn Tool>, ToolSource, ToolEffect)> {
        self.inner
            .specs(self.reserve_code || include_unavailable_code)
            .into_iter()
            .filter_map(|spec| {
                let kind = ToolKind::from_name(spec.name)?;
                let source = ToolSource::Native {
                    owner: OWNER.into(),
                    contract: spec.contract_id.into(),
                    trusted: true,
                };
                Some((
                    Arc::new(WorkcellTool {
                        kind,
                        spec,
                        host: Arc::clone(&self.inner),
                    }) as Arc<dyn Tool>,
                    source,
                    kind.effect(),
                ))
            })
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteHostRegistrationError {
    #[error("remote Workcell endpoint is also configured as generic MCP")]
    GenericMcpCollision,
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

#[derive(Clone)]
pub struct RemoteWorkcellHost {
    client: RemoteWorkcellClient,
}

impl RemoteWorkcellHost {
    pub fn new(client: RemoteWorkcellClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &RemoteWorkcellClient {
        &self.client
    }

    pub fn register(&self, registry: &ToolRegistry) -> Result<(), RemoteHostRegistrationError> {
        self.register_with_generic_mcp_endpoints(registry, std::iter::empty::<&str>())
    }

    pub fn register_with_generic_mcp_endpoints<I, S>(
        &self,
        registry: &ToolRegistry,
        endpoints: I,
    ) -> Result<(), RemoteHostRegistrationError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let selected = self.client.endpoint();
        if generic_mcp_endpoint_collision(selected, endpoints) {
            return Err(RemoteHostRegistrationError::GenericMcpCollision);
        }
        registry.register_many_audited(self.entries())?;
        Ok(())
    }

    fn entries(&self) -> Vec<(Arc<dyn Tool>, ToolSource, ToolEffect)> {
        canonical_remote_specs()
            .into_iter()
            .filter_map(|spec| {
                let verified = self.client.canonical_catalog().get(spec.name)?;
                let kind = ToolKind::from_name(spec.name)?;
                let source = self.source(verified);
                Some((
                    Arc::new(RemoteWorkcellTool {
                        kind,
                        spec: verified.clone(),
                        client: self.client.clone(),
                    }) as Arc<dyn Tool>,
                    source,
                    kind.effect(),
                ))
            })
            .collect()
    }

    fn source(&self, spec: &OwnedToolSpec) -> ToolSource {
        ToolSource::RemoteWorkcell {
            identity: Arc::new(RemotePermissionIdentity::from_binding(
                self.client.session_binding(),
            )),
            contract: format!(
                "{}@{}/{}",
                spec.contract_id, spec.contract_version, spec.result_version
            )
            .into(),
        }
    }
}

fn generic_mcp_endpoint_collision<I, S>(selected: &url::Url, endpoints: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    endpoints.into_iter().any(|endpoint| {
        url::Url::parse(endpoint.as_ref()).is_ok_and(|endpoint| endpoint == *selected)
    })
}

fn canonical_remote_specs() -> Vec<ToolSpec> {
    let mut expected = workcell::files::specs(ALLOW_WRITE);
    let year = jiff::Timestamp::now()
        .strftime("%Y")
        .to_string()
        .parse()
        .unwrap_or(2026);
    expected.extend(workcell::web::specs(
        year,
        &WebsearchExecutionConfiguration::default(),
    ));
    expected.extend(workcell::shell::specs());
    expected.extend(workcell::code_graph::specs());
    expected.extend(workcell::code::specs());
    expected.push(workcell::environment::spec());
    expected
}

fn canonical_remote_catalog(tools: &[OwnedToolSpec]) -> bool {
    let expected = canonical_remote_specs();
    let actual: Vec<_> = tools
        .iter()
        .filter(|spec| ToolKind::from_name(&spec.name).is_some())
        .collect();
    actual.len() == expected.len()
        && expected.iter().all(|expected| {
            actual
                .iter()
                .find(|actual| actual.name == expected.name)
                .is_some_and(|actual| {
                    actual.input_schema == expected.input_schema
                        && actual.output_schema == expected.output_schema
                        && actual.annotations == expected.annotations
                        && actual.presentation == expected.presentation
                        && actual.contract_id == expected.contract_id
                        && actual.contract_version == expected.contract_version
                        && actual.result_version == expected.result_version
                })
        })
}

struct RemoteWorkcellTool {
    kind: ToolKind,
    spec: OwnedToolSpec,
    client: RemoteWorkcellClient,
}

impl Tool for RemoteWorkcellTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(&self.spec.description)
    }

    fn schema(&self) -> Value {
        Value::Object(self.spec.input_schema.clone())
    }

    fn audience(&self) -> ToolAudience {
        self.kind.audience()
    }

    fn tool_kind(&self) -> Option<&str> {
        Some(self.kind.presentation_kind())
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        reject_remote_unknown_fields(&self.spec, input).map_err(ParseError::custom)?;
        let raw_input = input.clone();
        let input = Input::parse(self.kind, input.clone()).map_err(ParseError::custom)?;
        Ok(Box::new(RemoteWorkcellInvocation {
            client: self.client.clone(),
            kind: self.kind,
            input,
            raw_input,
            prepared: tokio::sync::Mutex::new(RemotePreparedState::default()),
        }))
    }
}

fn reject_remote_unknown_fields(spec: &OwnedToolSpec, input: &Value) -> Result<(), String> {
    let Some(input) = input.as_object() else {
        return Ok(());
    };
    let properties = spec
        .input_schema
        .get("properties")
        .and_then(Value::as_object);
    if let Some(field) = input
        .keys()
        .find(|field| properties.is_none_or(|properties| !properties.contains_key(*field)))
    {
        return Err(format!(
            "Invalid arguments for tool {}: unknown field `{field}`",
            spec.name
        ));
    }
    Ok(())
}

#[derive(Default)]
struct RemotePreparedState {
    call: Option<RemotePreparedToolCall>,
    execution_started: bool,
}

struct RemoteWorkcellInvocation {
    client: RemoteWorkcellClient,
    kind: ToolKind,
    input: Input,
    raw_input: Value,
    prepared: tokio::sync::Mutex<RemotePreparedState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    FileRead,
    FileGlob,
    FileGrep,
    FileWrite,
    FileEdit,
    FileApplyPatch,
    Index,
    Websearch,
    Webfetch,
    Shell,
    Code,
    CodeMap,
    CodeContext,
    CodeRefs,
    CodeImpact,
    CodeExpand,
    Environment,
}

impl ToolKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "file_read" => Some(Self::FileRead),
            "file_glob" => Some(Self::FileGlob),
            "file_grep" => Some(Self::FileGrep),
            "file_write" => Some(Self::FileWrite),
            "file_edit" => Some(Self::FileEdit),
            "file_apply_patch" => Some(Self::FileApplyPatch),
            "file_index" => Some(Self::Index),
            "websearch" => Some(Self::Websearch),
            "webfetch" => Some(Self::Webfetch),
            "shell" => Some(Self::Shell),
            "python_execution" => Some(Self::Code),
            "code_map" => Some(Self::CodeMap),
            "code_context" => Some(Self::CodeContext),
            "code_refs" => Some(Self::CodeRefs),
            "code_impact" => Some(Self::CodeImpact),
            "code_expand" => Some(Self::CodeExpand),
            "execution_environment" => Some(Self::Environment),
            _ => None,
        }
    }

    fn audience(self) -> ToolAudience {
        let read = ToolAudience::MAIN | ToolAudience::RESEARCH_SUB | ToolAudience::GENERAL_SUB;
        match self {
            Self::Index => ToolAudience::all(),
            Self::CodeMap | Self::CodeContext | Self::CodeRefs | Self::CodeImpact => {
                ToolAudience::all()
            }
            Self::FileWrite | Self::FileEdit | Self::FileApplyPatch => {
                ToolAudience::MAIN | ToolAudience::GENERAL_SUB
            }
            // Shell reaches a read-only agent too: dispatch refuses every call
            // that is not a confined read, and without it such an agent cannot
            // run `git diff`, which is most of what a reviewer needs to read.
            _ => read,
        }
    }

    fn presentation_kind(self) -> &'static str {
        match self {
            Self::FileRead | Self::Index | Self::CodeExpand => "read",
            Self::FileGlob
            | Self::FileGrep
            | Self::Websearch
            | Self::CodeMap
            | Self::CodeContext
            | Self::CodeRefs
            | Self::CodeImpact => "search",
            Self::FileWrite | Self::FileEdit | Self::FileApplyPatch => "edit",
            Self::Webfetch => "fetch",
            Self::Shell | Self::Code | Self::Environment => "execute",
        }
    }

    fn effect(self) -> ToolEffect {
        match self {
            Self::FileRead
            | Self::FileGlob
            | Self::FileGrep
            | Self::Index
            | Self::Websearch
            | Self::Webfetch
            | Self::CodeMap
            | Self::CodeContext
            | Self::CodeRefs
            | Self::CodeImpact
            | Self::CodeExpand => ToolEffect::ReadOnly,
            Self::Code => ToolEffect::Isolated,
            Self::FileWrite
            | Self::FileEdit
            | Self::FileApplyPatch
            | Self::Shell
            | Self::Environment => ToolEffect::Mutating,
        }
    }
}

struct WorkcellTool {
    kind: ToolKind,
    spec: ToolSpec,
    host: Arc<HostInner>,
}

impl Tool for WorkcellTool {
    fn name(&self) -> &str {
        self.spec.name
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(&self.spec.description)
    }

    fn schema(&self) -> Value {
        Value::Object(self.spec.input_schema.clone())
    }

    fn audience(&self) -> ToolAudience {
        self.kind.audience()
    }

    fn tool_kind(&self) -> Option<&str> {
        Some(self.kind.presentation_kind())
    }

    fn has_read_only_calls(&self) -> bool {
        self.kind == ToolKind::Shell
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        reject_unknown_fields(&self.spec, input).map_err(ParseError::custom)?;
        let raw_input = (self.kind == ToolKind::Shell).then(|| input.clone());
        let input = Input::parse(self.kind, input.clone()).map_err(ParseError::custom)?;
        Ok(Box::new(WorkcellInvocation {
            host: Arc::clone(&self.host),
            input,
            raw_input,
            prepared: Mutex::new(None),
        }))
    }
}

/// The outbound proxy for the web tools, read from the ambient environment.
///
/// Workcell already forwards these variables to every `shell` child, so without
/// this the two disagree: behind an enforcing proxy `shell curl` would reach the
/// network and `webfetch` would not.
///
/// An unusable value never fails host construction. Refusing to start over a
/// malformed `NO_PROXY` entry would take the whole session down for a setting
/// that only affects two tools, so it degrades to a direct dial and says so.
fn ambient_proxy() -> (ProxyConfiguration, Option<String>) {
    let first = |names: &[&str]| names.iter().find_map(|name| std::env::var(name).ok());
    match ProxyConfiguration::from_values(
        first(PROXY_HTTP_VARS).as_deref(),
        first(PROXY_HTTPS_VARS).as_deref(),
        first(PROXY_ALL_VARS).as_deref(),
        first(PROXY_BYPASS_VARS).as_deref(),
    ) {
        Ok(proxy) => (proxy, None),
        // The error never carries the value: a proxy URL may hold credentials.
        Err(error) => (
            ProxyConfiguration::direct(),
            Some(format!(
                "Workcell web tools are dialling directly: the proxy environment is unusable ({error})"
            )),
        ),
    }
}

fn reject_unknown_fields(spec: &ToolSpec, input: &Value) -> Result<(), String> {
    let Some(input) = input.as_object() else {
        return Ok(());
    };
    let properties = spec
        .input_schema
        .get("properties")
        .and_then(Value::as_object);
    if let Some(field) = input
        .keys()
        .find(|field| properties.is_none_or(|properties| !properties.contains_key(*field)))
    {
        return Err(format!(
            "Invalid arguments for tool {}: unknown field `{field}`",
            spec.name
        ));
    }
    Ok(())
}

#[derive(Clone)]
enum Input {
    FileRead(FileReadInput),
    FileGlob(FileGlobInput),
    FileGrep(FileGrepInput),
    FileWrite(FileWriteInput),
    FileEdit(FileEditInput),
    FileApplyPatch(FileApplyPatchInput),
    Index(IndexInput),
    Websearch(WebsearchInput),
    Webfetch(WebfetchInput),
    Shell(ShellInput),
    Code(CodeInput),
    CodeMap(CodeMapInput),
    CodeContext(CodeContextInput),
    CodeRefs(CodeRefsInput),
    CodeImpact(CodeImpactInput),
    CodeExpand(CodeExpandInput),
    Environment,
}

impl Input {
    fn shell_timeout(&self) -> Option<Duration> {
        match self {
            Self::Shell(input) => input.timeout_ms().ok().map(Duration::from_millis),
            _ => None,
        }
    }

    fn parse(kind: ToolKind, input: Value) -> Result<Self, String> {
        match kind {
            ToolKind::FileRead => parse_input("file_read", input).map(Self::FileRead),
            ToolKind::FileGlob => parse_input("file_glob", input).map(Self::FileGlob),
            ToolKind::FileGrep => parse_input("file_grep", input).map(Self::FileGrep),
            ToolKind::FileWrite => parse_input("file_write", input).map(Self::FileWrite),
            ToolKind::FileEdit => parse_input("file_edit", input).map(Self::FileEdit),
            ToolKind::FileApplyPatch => {
                parse_input("file_apply_patch", input).map(Self::FileApplyPatch)
            }
            ToolKind::Index => parse_input("file_index", input).map(Self::Index),
            ToolKind::Websearch => parse_input("websearch", input).map(Self::Websearch),
            ToolKind::Webfetch => parse_input("webfetch", input).map(Self::Webfetch),
            ToolKind::Shell => parse_input("shell", input).map(Self::Shell),
            ToolKind::Code => parse_input("python_execution", input).map(Self::Code),
            ToolKind::CodeMap => parse_input("code_map", input).map(Self::CodeMap),
            ToolKind::CodeContext => parse_input("code_context", input).map(Self::CodeContext),
            ToolKind::CodeRefs => parse_input("code_refs", input).map(Self::CodeRefs),
            ToolKind::CodeImpact => parse_input("code_impact", input).map(Self::CodeImpact),
            ToolKind::CodeExpand => parse_input("code_expand", input).map(Self::CodeExpand),
            ToolKind::Environment => match input {
                Value::Object(values) if values.is_empty() => Ok(Self::Environment),
                _ => Err(
                    "Invalid arguments for tool execution_environment: expected an empty object"
                        .into(),
                ),
            },
        }
    }
}

fn parse_input<T: DeserializeOwned>(name: &str, input: Value) -> Result<T, String> {
    serde_json::from_value(input)
        .map_err(|error| format!("Invalid arguments for tool {name}: {error}"))
}

enum PreparedExecution {
    File(FileToolGroup, Input),
    FileRead(FileToolGroup, PreparedFileRead),
    DirectoryRead(FileToolGroup, PreparedFileRead),
    Index(FileToolGroup, FileResource),
    FilePatch(FileToolGroup, PreparedFilePatch),
    Websearch(PreparedWebsearch),
    Webfetch(PreparedWebfetch),
    Shell(ShellToolGroup, Box<PreparedShell>),
    CodeGraph(Arc<CodeGraphToolGroup>),
    Environment(Arc<ExecutionEnvironment>),
    None,
}

struct PreparedInvocation {
    intent: PermissionIntent,
    execution: PreparedExecution,
    mutation_targets: Vec<PathBuf>,
    read_targets: Vec<PathBuf>,
}

struct WorkcellInvocation {
    host: Arc<HostInner>,
    input: Input,
    raw_input: Option<Value>,
    prepared: Mutex<Option<PreparedInvocation>>,
}

impl WorkcellInvocation {
    async fn prepare(&self, ctx: &ToolContext) -> Result<PermissionIntent, ToolError> {
        if let Some(prepared) = self
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            return Ok(prepared.intent.clone());
        }
        let project = ctx.permissions.project_cwd();
        let prepared = match &self.input {
            Input::FileRead(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let inspection_input = input.clone();
                let (group, read) = self
                    .host
                    .run(ctx, move |token| async move {
                        let groups = host.project_groups(cwd).await?;
                        let read = groups
                            .files
                            .prepare_read(inspection_input.clone(), &token)
                            .await
                            .map_err(filesystem_error)?;
                        if read.resource().access != FileResourceAccess::Traverse {
                            return Ok::<_, ToolError>((groups.files, read));
                        }
                        let group = FileToolGroup::new(
                            &read.resource().path,
                            false,
                            Some(*groups.files.limits()),
                        )
                        .await
                        .map_err(filesystem_error)?;
                        let read = group
                            .prepare_read(
                                FileReadInput {
                                    file_path: read.resource().path.to_string_lossy().into_owned(),
                                    ..inspection_input
                                },
                                &token,
                            )
                            .await
                            .map_err(filesystem_error)?;
                        Ok((group, read))
                    })
                    .await??;
                file_read_prepared(&project, group, read)
            }
            Input::FileGlob(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let inspection_input = input.clone();
                let (group, resource, search_path) = self
                    .host
                    .run(ctx, move |_| async move {
                        let groups = host.project_groups(cwd).await?;
                        let resource = groups
                            .files
                            .inspect_glob(&inspection_input)
                            .await
                            .map_err(filesystem_error)?;
                        let (group, search_path) =
                            confined_traversal_group(groups.files, &resource)
                                .await
                                .map_err(filesystem_error)?;
                        Ok::<_, ToolError>((group, resource, search_path))
                    })
                    .await??;
                let mut authorized = input.clone();
                authorized.path = Some(search_path);
                file_prepared(
                    vec![resource],
                    &project,
                    group,
                    Input::FileGlob(authorized),
                    &["/pattern"],
                )
            }
            Input::FileGrep(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let inspection_input = input.clone();
                let (group, resource, search_path) = self
                    .host
                    .run(ctx, move |_| async move {
                        let groups = host.project_groups(cwd).await?;
                        let resource = groups
                            .files
                            .inspect_grep(&inspection_input)
                            .await
                            .map_err(filesystem_error)?;
                        let (group, search_path) =
                            confined_traversal_group(groups.files, &resource)
                                .await
                                .map_err(filesystem_error)?;
                        Ok::<_, ToolError>((group, resource, search_path))
                    })
                    .await??;
                let mut authorized = input.clone();
                authorized.path = Some(search_path);
                file_prepared(
                    vec![resource],
                    &project,
                    group,
                    Input::FileGrep(authorized),
                    &["/pattern", "/include"],
                )
            }
            Input::FileWrite(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let inspection_input = input.clone();
                let (group, resource) = self
                    .host
                    .run(ctx, move |_| async move {
                        let groups = host.project_groups(cwd).await?;
                        let resource = groups
                            .files
                            .inspect_write(&inspection_input)
                            .await
                            .map_err(filesystem_error)?;
                        Ok::<_, ToolError>((groups.files, resource))
                    })
                    .await??;
                let mut authorized = input.clone();
                authorized.file_path = resource.path.to_string_lossy().into_owned();
                file_prepared(
                    vec![resource],
                    &project,
                    group,
                    Input::FileWrite(authorized),
                    &[],
                )
            }
            Input::FileEdit(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let inspection_input = input.clone();
                let (group, resource) = self
                    .host
                    .run(ctx, move |_| async move {
                        let groups = host.project_groups(cwd).await?;
                        let resource = groups
                            .files
                            .inspect_edit(&inspection_input)
                            .await
                            .map_err(filesystem_error)?;
                        Ok::<_, ToolError>((groups.files, resource))
                    })
                    .await??;
                let mut authorized = input.clone();
                authorized.file_path = resource.path.to_string_lossy().into_owned();
                file_prepared(
                    vec![resource],
                    &project,
                    group,
                    Input::FileEdit(authorized),
                    &[],
                )
            }
            Input::FileApplyPatch(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let patch_input = input.clone();
                let planned = self
                    .host
                    .run(ctx, move |token| async move {
                        let groups = host.project_groups(cwd).await?;
                        let patch = groups
                            .files
                            .prepare_apply_patch(patch_input, &token)
                            .await
                            .map_err(filesystem_error)?;
                        Ok::<_, ToolError>((groups.files, patch))
                    })
                    .await?;
                // Planning is where Workcell matches context, so a patch built
                // against a stale copy fails here rather than in execution.
                let (group, patch) = match planned {
                    Ok(planned) => planned,
                    Err(error) => {
                        let targets = patch::paths(&input.patch_text)
                            .into_iter()
                            .map(|path| project.join(path))
                            .collect::<Vec<_>>();
                        return Err(ToolError::new(
                            error.failure,
                            with_stale_notice(error.message, stale_notice(ctx, &targets)),
                        ));
                    }
                };
                let resources = patch.resources().to_vec();
                file_patch_prepared(resources, &project, group, patch)
            }
            Input::Index(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let mut inspection_input = input.clone();
                inspection_input.path = expand_tilde(&inspection_input.path)?;
                let (group, resource) = self
                    .host
                    .run(ctx, move |_| async move {
                        let groups = host.project_groups(cwd).await?;
                        let resource = groups
                            .files
                            .inspect_index(&inspection_input)
                            .await
                            .map_err(filesystem_error)?;
                        Ok::<_, ToolError>((groups.files, resource))
                    })
                    .await??;
                index_prepared(resource, &project, group)
            }
            Input::Websearch(input) => {
                let prepared = self
                    .host
                    .web
                    .prepare_websearch(input.clone())
                    .map_err(invalid_input)?;
                let intent = editor_adapter::web_intent(
                    PermissionResourceKind::Query,
                    prepared.permission_query.clone(),
                );
                PreparedInvocation {
                    intent,
                    execution: PreparedExecution::Websearch(prepared),
                    mutation_targets: Vec::new(),
                    read_targets: Vec::new(),
                }
            }
            Input::Webfetch(input) => {
                let prepared = self
                    .host
                    .web
                    .prepare_webfetch(input.clone())
                    .map_err(webfetch_error)?;
                let intent = editor_adapter::web_intent(
                    PermissionResourceKind::Url,
                    prepared.permission_url.clone(),
                );
                PreparedInvocation {
                    intent,
                    execution: PreparedExecution::Webfetch(prepared),
                    mutation_targets: Vec::new(),
                    read_targets: Vec::new(),
                }
            }
            Input::Shell(input) => {
                let host = Arc::clone(&self.host);
                let cwd = project.clone();
                let input = input.clone();
                let output_filter = ctx.config.shell_output_filter;
                let (group, shell) = self
                    .host
                    .run(ctx, move |_| async move {
                        let groups = host.project_groups(cwd).await?;
                        let group = groups.shell.with_output_filter(output_filter);
                        let prepared = group
                            .prepare(input)
                            .await
                            .map_err(shell_preparation_error)?;
                        Ok::<_, ToolError>((group, prepared))
                    })
                    .await??;
                shell_prepared(
                    group,
                    shell,
                    &project,
                    self.raw_input.as_ref(),
                    ctx.config.shell_native_redirect,
                    ctx.config.shell_workdir_redirect,
                )?
            }
            Input::Code(_) => exact_custom_prepared(
                "isolated_compute",
                "python",
                PermissionResourceAccess::Execute,
                PermissionRisk::Low,
            ),
            Input::CodeMap(input) => {
                let group = self.code_graph_group(ctx, project.clone()).await?;
                code_graph_prepared(group, &project, input.path.as_deref(), &["/path"])
            }
            Input::CodeContext(input) => {
                let group = self.code_graph_group(ctx, project.clone()).await?;
                code_graph_prepared(group, &project, input.path.as_deref(), &["/path", "/task"])
            }
            Input::CodeRefs(input) => {
                let group = self.code_graph_group(ctx, project.clone()).await?;
                code_graph_prepared(
                    group,
                    &project,
                    input.path.as_deref(),
                    &["/path", "/symbol"],
                )
            }
            Input::CodeImpact(input) => {
                let group = self.code_graph_group(ctx, project.clone()).await?;
                code_graph_prepared(
                    group,
                    &project,
                    input.path.as_deref(),
                    &["/path", "/symbol"],
                )
            }
            Input::CodeExpand(input) => {
                let group = self.code_graph_group(ctx, project.clone()).await?;
                code_graph_prepared(
                    group,
                    &project,
                    input.path.as_deref(),
                    &["/path", "/symbol"],
                )
            }
            Input::Environment => {
                let host = Arc::clone(&self.host);
                let environment = self
                    .host
                    .run(ctx, move |_| async move {
                        Ok::<_, ToolError>(host.project_groups(project).await?.environment)
                    })
                    .await??;
                let mut prepared = exact_custom_prepared(
                    "host_inspection",
                    "execution_environment",
                    PermissionResourceAccess::Read,
                    PermissionRisk::Medium,
                );
                prepared.execution = PreparedExecution::Environment(environment);
                prepared
            }
        };
        if let Some(missing) = missing_read_target(&prepared.intent) {
            return Err(ToolError::new(
                ToolFailure::NotFound,
                format!("{MISSING_READ_TARGET}: {missing}"),
            ));
        }
        let intent = prepared.intent.clone();
        *self
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(prepared);
        Ok(intent)
    }

    async fn code_graph_group(
        &self,
        ctx: &ToolContext,
        project: PathBuf,
    ) -> Result<Arc<CodeGraphToolGroup>, ToolError> {
        let host = Arc::clone(&self.host);
        self.host
            .run(ctx, move |_| async move {
                Ok::<_, ToolError>(host.project_groups(project).await?.code_graph)
            })
            .await?
    }

    async fn take_prepared(&self, ctx: &ToolContext) -> Result<PreparedInvocation, ToolError> {
        self.prepare(ctx).await?;
        self.prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| {
                ToolError::new(
                    ToolFailure::Other,
                    "Workcell invocation preparation was already consumed",
                )
            })
    }

    fn prepared_targets(&self, select: impl Fn(&PreparedInvocation) -> &[PathBuf]) -> Vec<PathBuf> {
        self.prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|prepared| select(prepared).to_vec())
            .unwrap_or_default()
    }
}

/// A bare pattern says nothing about where it ran, and the search root is the
/// difference between a repo-wide sweep and one directory.
fn search_header(pattern: &str, path: Option<&str>) -> String {
    match path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => format!("{pattern} in {path}"),
        None => pattern.to_owned(),
    }
}

fn input_header(input: &Input) -> String {
    match input {
        Input::FileRead(input) => input.file_path.clone(),
        Input::FileGlob(input) => search_header(&input.pattern, input.path.as_deref()),
        Input::FileGrep(input) => search_header(&input.pattern, input.path.as_deref()),
        Input::FileWrite(input) => input.file_path.clone(),
        Input::FileEdit(input) => input.file_path.clone(),
        Input::FileApplyPatch(input) => patch::header(&input.patch_text),
        Input::Index(input) => input.path.clone(),
        Input::Websearch(input) => input.query.clone(),
        Input::Webfetch(input) => input.url.clone(),
        Input::Shell(input) => input.command.lines().next().unwrap_or_default().into(),
        Input::Code(input) => format!("{} lines", input.code.lines().count()),
        Input::CodeMap(input) => input.path.clone().unwrap_or_else(|| ".".into()),
        Input::CodeContext(input) => search_header(&input.task, input.path.as_deref()),
        Input::CodeRefs(input) => search_header(&input.symbol, input.path.as_deref()),
        Input::CodeImpact(input) => search_header(&input.symbol, input.path.as_deref()),
        Input::CodeExpand(input) => search_header(&input.symbol, input.path.as_deref()),
        Input::Environment => "execution environment".into(),
    }
}

/// The deadline a command will really run under, so a header can name it
/// before it fires rather than after. `tool` is the canonical name; the caller
/// resolves whatever qualifier the call arrived with.
///
/// `None` when the executor would refuse the input, because naming a deadline
/// that never applies is worse than naming none. The input is decoded as the
/// executor's own type and asked for its deadline, so the rule has one home.
pub fn effective_timeout(tool: &str, raw_input: &Value) -> Option<Duration> {
    let millis = match tool {
        SHELL_TOOL_NAME => ShellInput::deserialize(raw_input).ok()?.timeout_ms(),
        PYTHON_EXECUTION_TOOL_NAME => CodeInput::deserialize(raw_input).ok()?.timeout_ms(),
        _ => return None,
    };
    millis.ok().map(Duration::from_millis)
}

/// The directory a command will start in, spelled the way its result's
/// `relative_workdir` will spell it: [`CURRENT_WORKDIR`] for `cwd` itself,
/// relative beneath it, absolute anywhere else. A header can then name the
/// directory before the call runs and keep the same words once it lands.
///
/// Lexical where the executor canonicalizes, so a symlink reads as written
/// until the result names where it led. `None` for a tool that takes no
/// workdir, and for a value that is not a path at all.
pub fn requested_workdir(tool: &str, raw_input: &Value, cwd: &Path) -> Option<String> {
    if tool != SHELL_TOOL_NAME {
        return None;
    }
    let requested = match raw_input.get("workdir") {
        None | Some(Value::Null) => "",
        Some(value) => value.as_str()?,
    };
    let mut resolved = PathBuf::new();
    for component in cwd.join(requested).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    let spelled = match resolved.strip_prefix(cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => {
            return Some(CURRENT_WORKDIR.to_owned());
        }
        Ok(relative) => relative,
        Err(_) => &resolved,
    };
    Some(spelled.to_string_lossy().replace('\\', "/"))
}

fn input_start_input(input: &Input) -> Option<ToolInput> {
    match input {
        Input::Shell(input) => Some(ToolInput::Code {
            language: "bash".into(),
            code: input.command.clone(),
        }),
        Input::Code(input) => Some(ToolInput::Code {
            language: "python".into(),
            code: input.code.clone(),
        }),
        _ => None,
    }
}

impl RemoteWorkcellInvocation {
    async fn prepare(&self, ctx: &ToolContext) -> Result<PermissionIntent, ToolError> {
        let mut state = self.prepared.lock().await;
        if let Some(call) = &state.call {
            return Ok(self.permission_intent(call));
        }
        let session = ctx
            .workspace_session
            .as_ref()
            .ok_or_else(|| ToolError::new(ToolFailure::Other, REMOTE_CONTEXT_UNAVAILABLE))?;
        let call = self
            .client
            .prepare_canonical_tool(
                session.binding(),
                session.cursor(),
                &ToolPrepareRequest {
                    name: self.kind.name().to_owned(),
                    input: self.raw_input.clone(),
                },
            )
            .await
            .map_err(workspace_error)?;
        let intent = self.permission_intent(&call);
        state.call = Some(call);
        Ok(intent)
    }

    fn permission_intent(&self, call: &RemotePreparedToolCall) -> PermissionIntent {
        let intent = &call.intent;
        let identity = RemotePermissionIdentity::from_binding(&call.binding);
        let resources = intent
            .resources
            .iter()
            .map(|resource| {
                let display = bounded_remote_display(resource.display.as_str());
                let remote_kind = match self.kind {
                    ToolKind::FileRead
                    | ToolKind::FileGlob
                    | ToolKind::FileGrep
                    | ToolKind::FileWrite
                    | ToolKind::FileEdit
                    | ToolKind::FileApplyPatch
                    | ToolKind::Index
                    | ToolKind::CodeMap
                    | ToolKind::CodeContext
                    | ToolKind::CodeRefs
                    | ToolKind::CodeImpact
                    | ToolKind::CodeExpand => None,
                    ToolKind::Websearch => Some("query"),
                    ToolKind::Webfetch => Some("url"),
                    ToolKind::Shell => match resource.access {
                        host_contract::ResourceAccess::Traverse => None,
                        host_contract::ResourceAccess::Inspect => Some("environment"),
                        _ => Some("command"),
                    },
                    ToolKind::Code => Some("code"),
                    ToolKind::Environment => Some("environment"),
                };
                let kind = if let Some(resource_kind) = remote_kind {
                    PermissionResourceKind::RemoteResource {
                        identity: identity.clone(),
                        resource_kind: resource_kind.into(),
                    }
                } else if matches!(
                    resource.access,
                    host_contract::ResourceAccess::Search | host_contract::ResourceAccess::Traverse
                ) || matches!(
                    self.kind,
                    ToolKind::FileGlob
                        | ToolKind::FileGrep
                        | ToolKind::Index
                        | ToolKind::CodeMap
                        | ToolKind::CodeContext
                        | ToolKind::CodeRefs
                        | ToolKind::CodeImpact
                        | ToolKind::CodeExpand
                ) {
                    PermissionResourceKind::RemoteDirectory {
                        identity: identity.clone(),
                    }
                } else {
                    PermissionResourceKind::RemoteFile {
                        identity: identity.clone(),
                    }
                };
                let access = match resource.access {
                    host_contract::ResourceAccess::Inspect
                    | host_contract::ResourceAccess::Read => PermissionResourceAccess::Read,
                    host_contract::ResourceAccess::Search
                    | host_contract::ResourceAccess::Traverse => PermissionResourceAccess::Search,
                    host_contract::ResourceAccess::Write
                    | host_contract::ResourceAccess::ReadWrite
                    | host_contract::ResourceAccess::Delete => PermissionResourceAccess::Write,
                    host_contract::ResourceAccess::Execute => PermissionResourceAccess::Execute,
                    host_contract::ResourceAccess::Connect => PermissionResourceAccess::Connect,
                };
                let mut attributes = BTreeMap::new();
                let display_attribute = match self.kind {
                    ToolKind::Websearch => "display_query",
                    ToolKind::Webfetch => "display_url",
                    ToolKind::Shell => match resource.access {
                        host_contract::ResourceAccess::Traverse => "display_path",
                        host_contract::ResourceAccess::Inspect => "display_environment",
                        _ => "display_command",
                    },
                    ToolKind::Code => "display_code",
                    ToolKind::Environment => "display_environment",
                    _ => "display_path",
                };
                attributes.insert(display_attribute.into(), display);
                attributes.insert(
                    "operation_kind".into(),
                    format!("{:?}", intent.kind).to_lowercase(),
                );
                if let Some(revision) = &resource.revision {
                    attributes.insert(
                        "resource_revision".into(),
                        bounded_remote_display(revision.as_str()),
                    );
                }
                let scope = resource
                    .scope
                    .iter()
                    .map(workcell::host_contract::ResourceId::as_str)
                    .collect::<Vec<_>>()
                    .join("\u{1f}");
                PermissionResource {
                    kind,
                    value: scope,
                    access: Some(access),
                    protected: intent.mutating,
                    requires_prompt: intent.mutating,
                    attributes,
                }
            })
            .collect();
        let risk = if intent.mutating {
            PermissionRisk::High
        } else if matches!(
            intent.kind,
            host_contract::OperationKind::Execute | host_contract::OperationKind::Transfer
        ) {
            PermissionRisk::Medium
        } else {
            PermissionRisk::Low
        };
        PermissionIntent::new(
            PermissionScopes::single(
                serde_json::to_string(intent).unwrap_or_else(|_| self.kind.name().to_owned()),
            ),
            resources,
            risk,
        )
        .with_authority(PermissionAuthorityProfile::RemoteResource)
    }

    async fn take_prepared(&self) -> Result<RemotePreparedToolCall, String> {
        let mut state = self.prepared.lock().await;
        let call = state
            .call
            .take()
            .ok_or_else(|| "Remote Workcell invocation was not prepared".to_owned())?;
        state.execution_started = true;
        Ok(call)
    }

    async fn execute_remote(
        &self,
        ctx: &ToolContext,
        mut call: RemotePreparedToolCall,
        cleanup: &mut RemoteExecutionCleanup,
    ) -> ToolExecResult {
        let mut progress = RemoteProgress::new(ctx);
        let session = match ctx.workspace_session.as_ref() {
            Some(session) => session,
            None => return Err(REMOTE_CONTEXT_UNAVAILABLE.to_owned()).into(),
        };
        if session.binding() != &call.binding || session.cursor() != &call.cursor {
            let _ = self
                .client
                .release_canonical_tool(&call.binding, &call.cursor, &call.prepared)
                .await;
            return Err("Remote Workcell workspace context changed after preparation".into())
                .into();
        }
        let ceiling = remote_execution_ceiling(self.kind);
        let timeout = match ctx.deadline.remaining() {
            Ok(remaining) => remaining.unwrap_or(ceiling).min(ceiling),
            Err(_) => Duration::ZERO,
        };
        let deadline = Instant::now() + timeout;
        // A stopped call must not prepare on the host again to renew itself.
        if timeout.is_zero() {
            return ToolExecResult::failed(ToolFailure::Timeout, REMOTE_DEADLINE_BEFORE_DISPATCH);
        }
        if let Err(cancelled) = ctx.cancel.race(future::ready(())).await {
            return ToolExecResult::failed(ToolFailure::Cancelled, cancelled);
        }
        if let Err(error) = self.renew_lapsing_preparation(&mut call, cleanup).await {
            return failed(error);
        }
        let executed = race_remote_execution(
            ctx,
            deadline,
            &mut cleanup.execution_started,
            self.client.execute_canonical_tool_tracked(
                session.binding(),
                session.cursor(),
                &call.prepared,
            ),
        )
        .await;
        let mut status = match executed {
            Ok(Some(Ok(status))) => status,
            Ok(Some(Err(RemoteToolExecutionError::BeforeDispatch(error)))) => {
                cleanup.execution_started = false;
                return failed(workspace_error(error));
            }
            Err(cancelled) if !cleanup.execution_started => {
                return ToolExecResult::failed(ToolFailure::Cancelled, cancelled);
            }
            Ok(None) if !cleanup.execution_started => {
                return ToolExecResult::failed(
                    ToolFailure::Timeout,
                    REMOTE_DEADLINE_BEFORE_DISPATCH,
                );
            }
            _ => match self.reconcile(&call, &mut progress).await {
                Some(status) => status,
                None => return indeterminate_result(&call.prepared, REMOTE_INDETERMINATE),
            },
        };
        let mut delay = REMOTE_POLL_INITIAL;
        let mut replay_attempts = 0;
        loop {
            let incomplete = !progress.publish(&status);
            if incomplete {
                replay_attempts += 1;
                if replay_attempts > REMOTE_RECONCILE_MAX_POLLS {
                    let _ = self.client.abandon_operation(&call.prepared.operation);
                    return if call.intent.mutating {
                        indeterminate_result(&call.prepared, REMOTE_PROGRESS_GAP)
                    } else {
                        Err(REMOTE_PROGRESS_GAP.into()).into()
                    };
                }
            }
            if incomplete || matches!(status.state, OperationState::Running) {
                let polled = ctx
                    .cancel
                    .race(future::race(
                        async {
                            smol::Timer::after(delay).await;
                            Some(
                                self.client
                                    .canonical_tool_status_after(
                                        session.binding(),
                                        session.cursor(),
                                        &status.handle,
                                        Some(progress.after_sequence()),
                                    )
                                    .await,
                            )
                        },
                        async {
                            smol::Timer::at(deadline).await;
                            None
                        },
                    ))
                    .await;
                delay = (delay * 2).min(REMOTE_POLL_MAX);
                status = match polled {
                    Ok(Some(Ok(status))) => status,
                    _ => match self.reconcile(&call, &mut progress).await {
                        Some(status) => status,
                        None => return indeterminate_result(&call.prepared, REMOTE_INDETERMINATE),
                    },
                };
                continue;
            }
            match status.state {
                OperationState::Completed { result, .. } => {
                    let result = remote_result(self.kind, &self.input, result);
                    return if progress.reported_gap.is_some() {
                        result.with_annotation(Some(REMOTE_PROGRESS_GAP.into()))
                    } else {
                        result
                    };
                }
                OperationState::Failed { error, .. } => {
                    return remote_failure_result(&self.input, error);
                }
                OperationState::Cancelled {
                    side_effects_possible,
                } => {
                    let message = if side_effects_possible {
                        REMOTE_INDETERMINATE
                    } else {
                        "Remote Workcell execution cancelled"
                    };
                    return ToolExecResult::failed(remote_cancellation(ctx, deadline), message)
                        .with_annotation(
                            side_effects_possible
                                .then(|| "remote outcome may include mutations".into()),
                        );
                }
                OperationState::Forgotten | OperationState::NeverSeen => {
                    return indeterminate_result(&call.prepared, REMOTE_FORGOTTEN);
                }
                OperationState::Running | OperationState::Indeterminate { .. } => {
                    return indeterminate_result(&call.prepared, REMOTE_INDETERMINATE);
                }
                OperationState::Prepared => {
                    return Err("Remote Workcell execution did not start".into()).into();
                }
            }
        }
    }

    async fn renew_lapsing_preparation(
        &self,
        call: &mut RemotePreparedToolCall,
        cleanup: &mut RemoteExecutionCleanup,
    ) -> Result<(), ToolError> {
        if !call.expires_within(REMOTE_PREPARATION_RENEWAL) {
            return Ok(());
        }
        let request = ToolPrepareRequest {
            name: self.kind.name().to_owned(),
            input: self.raw_input.clone(),
        };
        let renewed = self
            .client
            .prepare_canonical_tool(&call.binding, &call.cursor, &request)
            .await
            .map_err(|error| {
                ToolError::new(
                    ToolFailure::from(&error),
                    format!("{REMOTE_PREPARATION_LAPSED}: {error}"),
                )
            })?;
        if renewed.intent != call.intent {
            let _ = self
                .client
                .release_canonical_tool(&renewed.binding, &renewed.cursor, &renewed.prepared)
                .await;
            return Err(REMOTE_PREPARATION_CHANGED.to_owned().into());
        }
        let lapsed = std::mem::replace(call, renewed);
        cleanup.call = Some(call.clone());
        let _ = self
            .client
            .release_canonical_tool(&lapsed.binding, &lapsed.cursor, &lapsed.prepared)
            .await;
        Ok(())
    }

    async fn reconcile(
        &self,
        call: &RemotePreparedToolCall,
        progress: &mut RemoteProgress,
    ) -> Option<OperationStatus<RemoteToolResultEnvelope>> {
        let _ = self.client.abandon_operation(&call.prepared.operation);
        future::race(
            async {
                let _ = self
                    .client
                    .cancel_canonical_tool(&call.binding, &call.cursor, &call.prepared.operation)
                    .await;
                let mut delay = REMOTE_POLL_INITIAL;
                for _ in 0..REMOTE_RECONCILE_MAX_POLLS {
                    smol::Timer::after(delay).await;
                    if let Ok(status) = self
                        .client
                        .canonical_tool_status_after(
                            &call.binding,
                            &call.cursor,
                            &call.prepared.operation,
                            Some(progress.after_sequence()),
                        )
                        .await
                        && progress.publish(&status)
                        && !matches!(
                            status.state,
                            OperationState::Running | OperationState::Prepared
                        )
                    {
                        return Some(status);
                    }
                    delay = (delay * 2).min(REMOTE_POLL_MAX);
                }
                None
            },
            async {
                smol::Timer::after(REMOTE_RECONCILE_TIMEOUT).await;
                None
            },
        )
        .await
    }
}

fn race_remote_execution<T>(
    ctx: &ToolContext,
    deadline: Instant,
    execution_started: &mut bool,
    execution: impl Future<Output = T>,
) -> impl Future<Output = Result<Option<T>, String>> {
    ctx.cancel.race(future::race(
        async move {
            if Instant::now() >= deadline {
                return None;
            }
            *execution_started = true;
            Some(execution.await)
        },
        async move {
            smol::Timer::at(deadline).await;
            None
        },
    ))
}

fn bounded_remote_display(value: &str) -> String {
    value.chars().take(REMOTE_DISPLAY_MAX_CHARS).collect()
}

/// The host files a remote write names, spelled as the agent spelled them.
/// Shell and python name none, as locally: what they touch is unknown until
/// they run.
fn remote_write_paths(input: &Input) -> Vec<&str> {
    match input {
        Input::FileWrite(input) => vec![input.file_path.as_str()],
        Input::FileEdit(input) => vec![input.file_path.as_str()],
        Input::FileApplyPatch(input) => patch::paths(&input.patch_text),
        _ => Vec::new(),
    }
}

/// The host files a remote write prepares against, joined to the workspace
/// path `cwd`. An absolute spelling keeps its own key, because the host never
/// reveals where its root is. Two writes spelling one file both ways can still
/// both prepare, and the host's publication check refuses the second instead
/// of losing it.
fn remote_write_keys(input: &Input, cwd: &str) -> Vec<LockKey> {
    remote_write_paths(input)
        .into_iter()
        .map(|path| LockKey::remote(cwd, path))
        .collect()
}

/// A write whose file changed after the host prepared it is refused before
/// anything is published, so the agent gets what a local write to a file that
/// changed since it was read gets.
fn remote_failure_message(input: &Input, error: OperationError) -> String {
    let paths = remote_write_paths(input);
    if error.code.as_str() == STALE_RESOURCE_CODE && !paths.is_empty() {
        stale_read_message(paths.join(", "))
    } else {
        error.message
    }
}

fn remote_failure_result(input: &Input, error: OperationError) -> ToolExecResult {
    let failure = ToolFailure::from_code(error.code.as_str());
    ToolExecResult::failed(failure, remote_failure_message(input, error))
        .with_annotation(Some("remote Workcell failure".into()))
}

/// A host reports only that an operation was cancelled. Caudra cancels one
/// for its caller or once its own deadline has passed, and only local state
/// can tell which.
fn remote_cancellation(ctx: &ToolContext, deadline: Instant) -> ToolFailure {
    if !ctx.cancel.is_cancelled() && Instant::now() >= deadline {
        ToolFailure::Timeout
    } else {
        ToolFailure::Cancelled
    }
}

/// The cwd and command of a remote shell call, when the host prepared it in the
/// shape Caudra reviews.
fn remote_shell_line(resources: &[host_contract::ResourceIntent]) -> Option<(&str, &str)> {
    let [cwd, command, startup] = resources else {
        return None;
    };
    if cwd.access != host_contract::ResourceAccess::Traverse
        || command.access != host_contract::ResourceAccess::Execute
        || startup.access != host_contract::ResourceAccess::Inspect
        || serde_json::from_str::<Value>(startup.display.as_str()).ok()
            != serde_json::to_value(REMOTE_SHELL_ASSUMPTIONS).ok()
    {
        return None;
    }
    Some((cwd.display.as_str(), command.display.as_str()))
}

/// Where each command of a remote line runs, under the root the host keeps to
/// itself.
fn remote_shell_contexts(program: &BashProgram, cwd: &str) -> BashCommandContexts {
    program.command_contexts_with_assumptions(
        &Path::new(UNREVEALED_ROOT).join(cwd),
        REMOTE_SHELL_ASSUMPTIONS,
    )
}

fn remote_shell_plan_access(call: &RemotePreparedToolCall) -> PlanModeAccess {
    let Some((cwd, command)) = remote_shell_line(&call.intent.resources) else {
        return PlanModeAccess::Refused;
    };
    let read_only = workcell::shell::bash::parse_bash(command)
        .ok()
        .is_some_and(|program| {
            let contexts = remote_shell_contexts(&program, cwd);
            let facts = pattern_analysis::shell_facts(&program, &contexts);
            facts.opacity.is_none()
                && !facts.commands.is_empty()
                && facts
                    .commands
                    .iter()
                    .all(|command| read_only_shell::scope_is_read_only(&command.scope))
        });
    if read_only {
        PlanModeAccess::ReadOnly
    } else {
        PlanModeAccess::Prompted
    }
}

fn remote_shell_scope(resources: &[host_contract::ResourceIntent]) -> Option<RecordScope> {
    let Some((cwd, command)) = remote_shell_line(resources) else {
        return Some(RecordScope::Workspace);
    };
    match workcell::shell::bash::parse_bash(command) {
        Ok(program) => shell_record_scope::remote_shell_record_scope(
            &program,
            &remote_shell_contexts(&program, cwd),
        ),
        Err(_) => Some(RecordScope::Workspace),
    }
}

/// The record a remote write needs. Only a spelling relative to the cursor
/// can be placed, because the host never reveals where its root is.
fn remote_write_scope(input: &Input, cwd: &str) -> RecordScope {
    let root = Path::new(UNREVEALED_ROOT);
    let base = root.join(cwd);
    remote_write_paths(input)
        .into_iter()
        .map(|path| match RecordedPath::of(&base.join(path), root) {
            RecordedPath::Inside(path) => Some(path),
            RecordedPath::Outside | RecordedPath::Unplaced => None,
        })
        .collect::<Option<BTreeSet<_>>>()
        .filter(|paths| !paths.is_empty())
        .map_or(RecordScope::Workspace, RecordScope::Paths)
}

struct RemoteExecutionCleanup {
    client: RemoteWorkcellClient,
    call: Option<RemotePreparedToolCall>,
    execution_started: bool,
}

impl Drop for RemoteExecutionCleanup {
    fn drop(&mut self) {
        let Some(call) = self.call.take() else { return };
        let client = self.client.clone();
        if !self.execution_started {
            smol::spawn(async move {
                let _ = client
                    .release_canonical_tool(&call.binding, &call.cursor, &call.prepared)
                    .await;
            })
            .detach();
            return;
        }
        smol::spawn(async move {
            let _ = client.abandon_operation(&call.prepared.operation);
            future::race(
                async {
                    let _ = client
                        .cancel_canonical_tool(
                            &call.binding,
                            &call.cursor,
                            &call.prepared.operation,
                        )
                        .await;
                    let mut after = 0;
                    for _ in 0..REMOTE_RECONCILE_MAX_POLLS {
                        if let Ok(status) = client
                            .canonical_tool_status_after(
                                &call.binding,
                                &call.cursor,
                                &call.prepared.operation,
                                Some(after),
                            )
                            .await
                        {
                            after = status.progress.last().map_or(after, |event| event.sequence);
                            if !matches!(
                                status.state,
                                OperationState::Running | OperationState::Prepared
                            ) {
                                break;
                            }
                        }
                        smol::Timer::after(REMOTE_POLL_INITIAL).await;
                    }
                },
                async {
                    smol::Timer::after(REMOTE_RECONCILE_TIMEOUT).await;
                },
            )
            .await;
        })
        .detach();
    }
}

/// A shell command may legitimately run for the whole deadline Workcell grants
/// it, and giving up first cancels the remote operation rather than reporting
/// it. Every other tool keeps the shorter ceiling.
const fn remote_execution_ceiling(kind: ToolKind) -> Duration {
    match kind {
        ToolKind::Shell => SHELL_EXECUTION_TIMEOUT,
        _ => REMOTE_EXECUTION_TIMEOUT,
    }
}

impl ToolKind {
    fn name(self) -> &'static str {
        NATIVE_TOOL_NAMES
            .iter()
            .copied()
            .find(|name| Self::from_name(name) == Some(self))
            .unwrap_or("unknown")
    }
}

impl ToolInvocation for RemoteWorkcellInvocation {
    fn shell_timeout(&self) -> Option<Duration> {
        self.input.shell_timeout()
    }

    fn runs_remotely(&self) -> bool {
        true
    }

    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(input_header(&self.input)))
    }

    fn start_input(&self) -> Option<ToolInput> {
        input_start_input(&self.input)
    }

    fn plan_mode_access(&self) -> PlanModeAccess {
        if self.kind == ToolKind::Shell {
            return self
                .prepared
                .try_lock()
                .ok()
                .and_then(|state| state.call.as_ref().map(remote_shell_plan_access))
                .unwrap_or(PlanModeAccess::Refused);
        }
        self.prepared
            .try_lock()
            .ok()
            .and_then(|state| state.call.as_ref().map(|call| call.intent.mutating))
            .map_or(PlanModeAccess::Refused, |mutating| {
                if mutating {
                    PlanModeAccess::Prompted
                } else {
                    PlanModeAccess::ReadOnly
                }
            })
    }

    fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
        if self.kind == ToolKind::Shell {
            return if self.plan_mode_access() == PlanModeAccess::ReadOnly {
                ToolEffect::ReadOnly
            } else {
                registered
            };
        }
        self.prepared
            .try_lock()
            .ok()
            .and_then(|state| state.call.as_ref().map(|call| call.intent.mutating))
            .map_or(registered, |mutating| {
                if mutating {
                    registered
                } else if self.kind == ToolKind::Code {
                    ToolEffect::Isolated
                } else {
                    ToolEffect::ReadOnly
                }
            })
    }

    /// Resolved against the session's cursor, never `{cwd}`: that names another
    /// directory for the main agent, a workflow agent and a headless run, and
    /// one file has to take one key in all of them. A stale cursor takes none,
    /// as its preparation is refused anyway.
    fn preflight_write_keys(&self, ctx: &ToolContext) -> Vec<LockKey> {
        ctx.workspace_session
            .as_ref()
            .and_then(|session| {
                self.client
                    .cursor_path(session.binding(), session.cursor())
                    .ok()
            })
            .map_or_else(Vec::new, |cwd| remote_write_keys(&self.input, cwd.as_str()))
    }

    /// The host runs the call under a root it keeps to itself, which `root`, a
    /// directory on this machine, does not name.
    fn record_scope(&self, ctx: &ToolContext, _root: &Path) -> Option<RecordScope> {
        if self.kind == ToolKind::Shell {
            return self
                .prepared
                .try_lock()
                .ok()
                .and_then(|state| {
                    state
                        .call
                        .as_ref()
                        .map(|call| remote_shell_scope(&call.intent.resources))
                })
                .unwrap_or(Some(RecordScope::Workspace));
        }
        let cwd = ctx.workspace_session.as_ref().and_then(|session| {
            self.client
                .cursor_path(session.binding(), session.cursor())
                .ok()
        });
        Some(cwd.map_or(RecordScope::Workspace, |cwd| {
            remote_write_scope(&self.input, cwd.as_str())
        }))
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async move { self.prepare(ctx).await.map(Some) })
    }

    fn permission_input(&self) -> Option<&Value> {
        Some(&self.raw_input)
    }

    fn abandon<'a>(&'a self, _ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let call = {
                let mut state = self.prepared.lock().await;
                if state.execution_started {
                    None
                } else {
                    state.call.take()
                }
            };
            if let Some(call) = call {
                let _ = self
                    .client
                    .release_canonical_tool(&call.binding, &call.cursor, &call.prepared)
                    .await;
            }
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let call = match self.take_prepared().await {
                Ok(call) => call,
                Err(error) => return Err(error).into(),
            };
            let mut cleanup = RemoteExecutionCleanup {
                client: self.client.clone(),
                call: Some(call.clone()),
                execution_started: false,
            };
            let result = self.execute_remote(ctx, call, &mut cleanup).await;
            if cleanup.execution_started {
                cleanup.call = None;
            }
            result
        })
    }
}

struct RemoteProgress {
    sink: Option<flume::Sender<ToolLive>>,
    buffer: Arc<SharedBuf>,
    buffer_published: bool,
    next_sequence: Option<u64>,
    reported_gap: Option<u64>,
}

impl RemoteProgress {
    fn new(ctx: &ToolContext) -> Self {
        let buffer = Arc::new(SharedBuf::new());
        if let Some(live) = &ctx.shell_live {
            live.attach(&buffer);
        }
        Self {
            sink: ctx.live_sink.clone(),
            buffer,
            buffer_published: false,
            next_sequence: None,
            reported_gap: None,
        }
    }

    fn after_sequence(&self) -> u64 {
        self.next_sequence.unwrap_or(1) - 1
    }

    /// Never waits on the card: a full or closed sink drops the update rather
    /// than stall the poll that produced it.
    fn offer(&self, live: ToolLive) {
        if let Some(sink) = &self.sink {
            let _ = sink.try_send(live);
        }
    }

    fn publish(&mut self, status: &OperationStatus<RemoteToolResultEnvelope>) -> bool {
        let terminal = matches!(
            status.state,
            OperationState::Completed { .. }
                | OperationState::Failed { .. }
                | OperationState::Cancelled { .. }
        );
        let first = status.progress_metadata.first_retained_sequence;
        let gap = (status.progress_metadata.gap_before_first && self.next_sequence.is_none())
            || self
                .next_sequence
                .zip(first)
                .is_some_and(|(expected, first)| first > expected);
        let gap_sequence = first.unwrap_or(status.progress_metadata.next_sequence);
        if gap && self.reported_gap != Some(gap_sequence) {
            self.offer(ToolLive::Annotation(REMOTE_PROGRESS_GAP.into()));
            self.reported_gap = Some(gap_sequence);
        }
        let mut expected = self.next_sequence.unwrap_or(1);
        if status.progress_metadata.gap_before_first {
            expected = expected.max(first.unwrap_or(status.progress_metadata.next_sequence));
            self.next_sequence = Some(expected);
        }
        for item in &status.progress {
            if item.sequence < expected {
                continue;
            }
            if item.sequence > expected && self.reported_gap != Some(item.sequence) {
                self.offer(ToolLive::Annotation(REMOTE_PROGRESS_GAP.into()));
                self.reported_gap = Some(item.sequence);
            }
            if item.sequence > expected && !status.progress_metadata.gap_before_first && !terminal {
                return false;
            }
            expected = item.sequence.saturating_add(1);
            self.next_sequence = Some(expected);
            match item.kind {
                OperationProgressKind::Stdout | OperationProgressKind::Stderr => {
                    self.buffer.append(SnapshotLine::plain(item.chunk.clone()));
                    if !self.buffer_published {
                        self.offer(ToolLive::Buf(Arc::clone(&self.buffer)));
                        self.buffer_published = true;
                    }
                }
                OperationProgressKind::Started
                | OperationProgressKind::Exited
                | OperationProgressKind::Unknown(_) => {
                    if !item.chunk.is_empty() {
                        self.offer(ToolLive::Annotation(item.chunk.clone()));
                    }
                }
            }
        }
        if terminal && expected != status.progress_metadata.next_sequence {
            self.reported_gap = Some(status.progress_metadata.next_sequence);
            self.offer(ToolLive::Annotation(REMOTE_PROGRESS_GAP.into()));
        }
        if status.progress_metadata.gap_before_first || terminal {
            self.next_sequence = Some(status.progress_metadata.next_sequence);
            true
        } else {
            expected == status.progress_metadata.next_sequence
        }
    }
}

fn indeterminate_result(prepared: &PreparedToolCall, message: &str) -> ToolExecResult {
    let invocation = prepared
        .operation
        .invocation_id
        .as_ref()
        .map_or("unknown", |id| id.as_str());
    ToolExecResult::from(Err(format!("{message} (invocation {invocation})"))).with_annotation(Some(
        "remote mutation outcome unknown; non-retryable".into(),
    ))
}

fn remote_result(
    kind: ToolKind,
    input: &Input,
    envelope: RemoteToolResultEnvelope,
) -> ToolExecResult {
    let RemoteToolResultEnvelope {
        model_output,
        structured_content,
        is_error,
    } = envelope;
    let parsed = match kind {
        ToolKind::FileRead => deserialize_remote(&structured_content)
            .map(|output| remote_file_read_result(output, &model_output)),
        ToolKind::FileGlob => deserialize_remote(&structured_content).map(file_glob_result),
        ToolKind::FileGrep => deserialize_remote(&structured_content).map(file_grep_result),
        ToolKind::FileWrite => deserialize_remote(&structured_content).map(|output| {
            let Input::FileWrite(input) = input else {
                unreachable!("tool kind and parsed input are paired")
            };
            file_write_result(output, input.content.clone())
        }),
        ToolKind::FileEdit => deserialize_remote(&structured_content).map(|output| {
            let Input::FileEdit(input) = input else {
                unreachable!("tool kind and parsed input are paired")
            };
            file_edit_result(
                output,
                input.old_string.clone(),
                input.new_string.clone(),
                input.replace_all.unwrap_or(false),
            )
        }),
        ToolKind::FileApplyPatch => deserialize_remote(&structured_content).map(file_patch_result),
        ToolKind::Index => deserialize_remote(&structured_content)
            .map(|output| remote_index_result(output, &model_output)),
        ToolKind::Shell => remote_shell_result(&structured_content, model_output.clone()),
        ToolKind::Code => Ok(remote_code_result(
            &structured_content,
            model_output.clone(),
        )),
        ToolKind::Websearch => Ok(text_result(
            &structured_content,
            model_output.clone(),
            true,
            model_output.clone(),
        )),
        ToolKind::Webfetch => {
            let markdown = structured_content["format"] == "markdown";
            Ok(text_result(
                &structured_content,
                model_output.clone(),
                markdown,
                model_output.clone(),
            ))
        }
        ToolKind::CodeMap
        | ToolKind::CodeContext
        | ToolKind::CodeRefs
        | ToolKind::CodeImpact
        | ToolKind::CodeExpand => {
            remote_code_graph_result(kind, structured_content, model_output.clone())
        }
        ToolKind::Environment => environment_card(&structured_content)
            .map(|output| ToolExecResult::from(Ok::<_, String>(output))),
    };
    match parsed {
        Ok(result) => {
            let is_error = is_error || result.is_error;
            result
                .with_error(is_error)
                .with_model_output(Some(model_output))
                .with_remote_written_paths()
        }
        Err(error) => Err(format!("invalid remote Workcell result: {error}")).into(),
    }
}

fn deserialize_remote<T: DeserializeOwned>(value: &Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(value.clone())
}

fn remote_file_read_result(output: FileReadOutput, model_output: &str) -> ToolExecResult {
    let output = match output {
        FileReadOutput::Directory {
            path,
            relative_path,
            entry_details,
            truncated,
            ..
        } => FileReadOutput::Directory {
            path,
            relative_path,
            entries: model_output.lines().map(str::to_owned).collect(),
            entry_details,
            truncated,
        },
        FileReadOutput::File {
            path,
            relative_path,
            text,
            line_start,
            line_end,
            total_lines,
            truncated,
            ..
        } => FileReadOutput::File {
            path,
            relative_path,
            numbered_text: model_output.to_owned(),
            text,
            line_start,
            line_end,
            total_lines,
            truncated,
        },
    };
    file_read_result(output)
}

fn remote_index_result(output: WorkcellIndexOutput, model_output: &str) -> ToolExecResult {
    let output = match output {
        WorkcellIndexOutput::File {
            path,
            relative_path,
            language,
            lines,
            source_line_count,
            parse_error,
            truncated,
            ..
        } => WorkcellIndexOutput::File {
            path,
            relative_path,
            language,
            skeleton: model_output.to_owned(),
            lines,
            source_line_count,
            parse_error,
            truncated,
        },
        WorkcellIndexOutput::Directory {
            path,
            relative_path,
            entries,
            total_count,
            truncated,
            ..
        } => WorkcellIndexOutput::Directory {
            path,
            relative_path,
            entries,
            total_count,
            truncated,
            listing: model_output.to_owned(),
        },
    };
    index_result(output, IndexLimits::default().max_model_output_bytes)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteShellOutput {
    relative_workdir: String,
    timeout_ms: u64,
    duration_ms: u64,
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    output_limit_exceeded: bool,
    final_sequence: u64,
    stdout_utf8_bytes: u64,
    stderr_utf8_bytes: u64,
    stdout: String,
    stderr: String,
    stdout_capture_truncated: bool,
    stderr_capture_truncated: bool,
    stdout_preview_truncated: bool,
    stderr_preview_truncated: bool,
    stdout_redraws_collapsed: u64,
    stderr_redraws_collapsed: u64,
}

fn remote_shell_result(
    value: &Value,
    model_output: String,
) -> Result<ToolExecResult, serde_json::Error> {
    let output: RemoteShellOutput = deserialize_remote(value)?;
    let output = AgentShellOutput {
        model_text: model_output.clone(),
        relative_workdir: output.relative_workdir,
        timeout_ms: output.timeout_ms,
        duration_ms: output.duration_ms,
        exit_code: output.exit_code,
        signal: output.signal,
        timed_out: output.timed_out,
        output_limit_exceeded: output.output_limit_exceeded,
        final_sequence: output.final_sequence,
        stdout_utf8_bytes: output.stdout_utf8_bytes,
        stderr_utf8_bytes: output.stderr_utf8_bytes,
        stdout: output.stdout,
        stderr: output.stderr,
        stdout_capture_truncated: output.stdout_capture_truncated,
        stderr_capture_truncated: output.stderr_capture_truncated,
        stdout_preview_truncated: output.stdout_preview_truncated,
        stderr_preview_truncated: output.stderr_preview_truncated,
        stdout_redraws_collapsed: output.stdout_redraws_collapsed,
        stderr_redraws_collapsed: output.stderr_redraws_collapsed,
        filter: None,
    };
    Ok(shell_exec_result(output, model_output))
}

fn remote_code_result(value: &Value, model_output: String) -> ToolExecResult {
    let result = text_result(value, model_output.clone(), false, model_output);
    let outcome = CODE_OUTCOMES.into_iter().find(|outcome| {
        serde_json::to_value(outcome).is_ok_and(|name| name == value[CODE_OUTCOME_FIELD])
    });
    match outcome {
        Some(outcome) => code_outcome_result(result, outcome, value[CODE_TIMED_OUT_FIELD] == true),
        None => result.with_failure(ToolFailure::Other),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteGraphSymbol {
    name: String,
    kind: String,
    path: String,
    line_start: usize,
    line_end: usize,
    callers: Option<usize>,
    calls: Option<usize>,
    test_scope: Option<bool>,
    hops: Option<usize>,
}

fn graph_rows(value: &Value) -> Result<Vec<CodeGraphRow>, serde_json::Error> {
    let symbols: Vec<RemoteGraphSymbol> = deserialize_remote(value)?;
    Ok(symbols
        .into_iter()
        .map(|symbol| CodeGraphRow {
            name: symbol.name,
            kind: symbol.kind,
            path: symbol.path,
            line_start: symbol.line_start,
            line_end: symbol.line_end,
            inbound: symbol.callers,
            outbound: symbol.calls,
            hops: symbol.hops,
            test_scope: symbol.test_scope.unwrap_or(false),
        })
        .collect())
}

fn remote_code_graph_result(
    kind: ToolKind,
    value: Value,
    model_output: String,
) -> Result<ToolExecResult, serde_json::Error> {
    if value["refused"] == true {
        let candidates = value["didYouMean"].as_array().map_or(0, Vec::len);
        return Ok(
            ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain(text_output(
                model_output.clone(),
                value,
            ))))
            .with_model_output(Some(model_output))
            .with_annotation(Some(if candidates == 0 {
                "no match".into()
            } else {
                format!("no match \u{b7} {candidates} candidates")
            })),
        );
    }
    let string = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
    let usize_value = |name: &str| value[name].as_u64().unwrap_or_default() as usize;
    let (headline, rows, source, annotation) = match kind {
        ToolKind::CodeMap => (
            format!("ranked symbols in {}", string("path")),
            graph_rows(&value["symbols"])?,
            None,
            shown_of(usize_value("shown"), usize_value("total"), "symbols"),
        ),
        ToolKind::CodeContext => (
            format!(
                "read as {} ({}); confidence {} at {}% separation",
                string("shape"),
                string("shapeReason"),
                string("confidence"),
                usize_value("marginPercent")
            ),
            graph_rows(&value["results"])?,
            None,
            format!(
                "{} \u{b7} {} confidence",
                shown_of(usize_value("shown"), usize_value("totalMatched"), "matches"),
                string("confidence")
            ),
        ),
        ToolKind::CodeRefs => (
            format!(
                "{} of {}, each row one {}",
                string("direction"),
                string("symbol"),
                string("unit")
            ),
            graph_rows(&value["references"])?,
            None,
            shown_of(usize_value("shown"), usize_value("total"), &string("unit")),
        ),
        ToolKind::CodeImpact => (
            format!(
                "{} symbols reach {} within {} hops; {} of them are tests",
                usize_value("total"),
                string("symbol"),
                usize_value("depth"),
                value["testsReaching"].as_array().map_or(0, Vec::len)
            ),
            graph_rows(&value["reached"])?,
            None,
            format!(
                "{} \u{b7} {} tests",
                shown_of(usize_value("shown"), usize_value("total"), "reached"),
                value["testsReaching"].as_array().map_or(0, Vec::len)
            ),
        ),
        ToolKind::CodeExpand => {
            let callers = graph_rows(&value["callers"])?;
            let callees = graph_rows(&value["callees"])?;
            let rows = callers
                .into_iter()
                .map(|mut row| {
                    row.inbound = Some(1);
                    row
                })
                .chain(callees.into_iter().map(|mut row| {
                    row.outbound = Some(1);
                    row
                }))
                .collect();
            let source_text = string("source");
            (
                format!(
                    "{} {} {}:{}-{}",
                    string("symbol"),
                    string("kind"),
                    string("path"),
                    usize_value("lineStart"),
                    usize_value("lineEnd")
                ),
                rows,
                Some(CodeGraphSource {
                    path: string("path"),
                    kind: string("kind"),
                    line_start: usize_value("lineStart"),
                    lines: source_text.lines().map(str::to_owned).collect(),
                    whole_file_reason: value["servedWholeFile"].as_str().map(str::to_owned),
                }),
                format!(
                    "{} callers \u{b7} {} callees",
                    value["callers"].as_array().map_or(0, Vec::len),
                    value["callees"].as_array().map_or(0, Vec::len)
                ),
            )
        }
        _ => unreachable!("only code graph tools use this adapter"),
    };
    let footer = model_output
        .lines()
        .next_back()
        .unwrap_or_default()
        .to_owned();
    Ok(ToolExecResult::from(Ok::<_, String>(ToolOutput::CodeGraph {
        headline,
        rows,
        source,
        footer,
        state: Some(value),
    }))
    .with_model_output(Some(model_output))
    .with_annotation(Some(annotation)))
}

impl ToolInvocation for WorkcellInvocation {
    fn shell_timeout(&self) -> Option<Duration> {
        self.input.shell_timeout()
    }

    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(input_header(&self.input)))
    }

    /// A patch is deliberately absent: its result is the same diff, rendered
    /// with real line numbers, so echoing the request above it says
    /// everything twice and truncates both halves.
    fn start_input(&self) -> Option<ToolInput> {
        input_start_input(&self.input)
    }

    fn mutation_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
        self.prepared_targets(|prepared| &prepared.mutation_targets)
    }

    /// A shell line records what its text names it writing. Every other tool
    /// records the files it declared.
    fn record_scope(&self, ctx: &ToolContext, root: &Path) -> Option<RecordScope> {
        if !matches!(self.input, Input::Shell(_)) {
            return RecordScope::of_files(&self.mutation_targets(ctx), root);
        }
        match self
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|prepared| &prepared.execution)
        {
            Some(PreparedExecution::Shell(_, shell)) => {
                shell_record_scope::prepared_shell_record_scope(shell, root)
            }
            _ => Some(RecordScope::Workspace),
        }
    }

    fn read_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
        self.prepared_targets(|prepared| &prepared.read_targets)
    }

    /// A shell line is judged from its parsed form, which only exists once
    /// `preflight` has run. Reaching here without it means the parse never
    /// happened, so the line is unreviewed and refused.
    fn plan_mode_access(&self) -> PlanModeAccess {
        if !matches!(self.input, Input::Shell(_)) {
            return PlanModeAccess::Standard;
        }
        match self
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|prepared| &prepared.execution)
        {
            Some(PreparedExecution::Shell(_, shell)) => editor_adapter::shell_plan_access(shell),
            _ => PlanModeAccess::Refused,
        }
    }

    /// A shell line counts as read-only only when preflight proved every one of
    /// its commands both harmless and confined to the project. An unparsed line
    /// contributes one opaque resource without the attribute, so a line nobody
    /// could read keeps the registered effect.
    fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
        if !matches!(self.input, Input::Shell(_)) {
            return registered;
        }
        let prepared = self
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(resources) = prepared
            .as_ref()
            .map(|prepared| &prepared.intent.resources)
            .filter(|resources| !resources.is_empty())
        else {
            return registered;
        };
        if resources
            .iter()
            .all(|resource| resource.attributes.contains_key(CONFINED_READ_ATTRIBUTE))
        {
            ToolEffect::ReadOnly
        } else {
            registered
        }
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async move { self.prepare(ctx).await.map(Some) })
    }

    fn permission_input(&self) -> Option<&Value> {
        self.raw_input.as_ref()
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let prepared = match self.take_prepared(ctx).await {
                Ok(prepared) => prepared,
                Err(error) => return failed(error),
            };
            self.execute_prepared(ctx, prepared).await
        })
    }
}

impl WorkcellInvocation {
    async fn execute_prepared(
        &self,
        ctx: &ToolContext,
        prepared: PreparedInvocation,
    ) -> ToolExecResult {
        match (&self.input, prepared.execution) {
            (Input::FileRead(_), PreparedExecution::DirectoryRead(group, read)) => {
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.execute_prepared_directory_read(read, &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => file_read_result(output),
                    Ok(Err(error)) => failed(filesystem_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::FileRead(_), PreparedExecution::FileRead(group, read)) => {
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.execute_prepared_read(read, &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        if let FileReadOutput::File { path, .. } = &output {
                            ctx.file_tracker.record_read(Path::new(path));
                        }
                        file_read_result(output)
                    }
                    Ok(Err(error)) => failed(filesystem_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::FileGlob(_), PreparedExecution::File(group, Input::FileGlob(input))) => {
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.file_glob(input, &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => file_glob_result(output),
                    Ok(Err(error)) => failed(filesystem_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::FileGrep(_), PreparedExecution::File(group, Input::FileGrep(input))) => {
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.file_grep(input, &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        let mut paths = HashSet::new();
                        for row in &output.rows {
                            if paths.insert(&row.path) {
                                ctx.file_tracker.record_read(Path::new(&row.path));
                            }
                        }
                        file_grep_result(output)
                    }
                    Ok(Err(error)) => failed(filesystem_error(error)),
                    Err(error) => failed(error),
                }
            }
            (
                Input::FileWrite(original),
                PreparedExecution::File(group, Input::FileWrite(input)),
            ) => {
                if let Some(error) = stale_notice(ctx, &prepared.mutation_targets) {
                    return Err(error).into();
                }
                let content = original.content.clone();
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.file_write(input, &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        if output.applied {
                            ctx.file_tracker.record_read(Path::new(&output.path));
                        }
                        file_write_result(output, content)
                    }
                    Ok(Err(error)) => failed(filesystem_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::FileEdit(original), PreparedExecution::File(group, Input::FileEdit(input))) => {
                // Captured before the edit runs: a successful write moves the
                // mtime itself, so checking afterwards would report staleness
                // this call caused.
                let stale = stale_notice(ctx, &prepared.mutation_targets);
                let old_string = original.old_string.clone();
                let new_string = original.new_string.clone();
                let replace_all = original.replace_all.unwrap_or(false);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.file_edit(input, &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        if output.applied {
                            ctx.file_tracker.record_read(Path::new(&output.path));
                        }
                        file_edit_result(output, old_string, new_string, replace_all)
                    }
                    Ok(Err(error)) => ToolExecResult::failed(
                        ToolFailure::from_code(error.code()),
                        with_stale_notice(error.to_string(), stale),
                    ),
                    Err(error) => failed(error),
                }
            }
            // No stale check: the patch only reaches execution once Workcell has
            // matched every context line at plan time, so a stale copy of the
            // file has already failed in `prepare`.
            (Input::FileApplyPatch(_), PreparedExecution::FilePatch(group, patch)) => {
                let result = self
                    .host
                    .run(ctx, move |token| async move {
                        group
                            .execute_prepared_patch(patch, &token)
                            .await
                            .map_err(filesystem_error)
                    })
                    .await
                    .flatten();
                match result {
                    Ok(output) => {
                        if output.applied {
                            for path in applied_patch_paths(&output) {
                                ctx.file_tracker.record_read(Path::new(&path));
                            }
                        }
                        file_patch_result(output)
                    }
                    Err(error) => failed(error),
                }
            }
            (Input::Index(_), PreparedExecution::Index(group, resource)) => {
                let Some(max_source_bytes) =
                    ctx.config.index_max_file_size_mb.checked_mul(BYTES_PER_MIB)
                else {
                    return Err("index max file size exceeds this platform's byte range".to_owned())
                        .into();
                };
                let limits = IndexLimits {
                    max_source_bytes,
                    ..IndexLimits::default()
                };
                let max_model_output_bytes = limits.max_model_output_bytes;
                let configuration = IndexExecutionConfiguration { limits };
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group
                            .index_authorized_with_configuration(resource, configuration, &token)
                            .await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        if let WorkcellIndexOutput::File { path, .. } = &output {
                            ctx.file_tracker.record_read(Path::new(path));
                        }
                        index_result(output, max_model_output_bytes)
                    }
                    Ok(Err(error)) => failed(filesystem_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::Websearch(_), PreparedExecution::Websearch(prepared)) => {
                match self
                    .host
                    .run(ctx, {
                        let web = self.host.web.clone();
                        // A search reports being stopped only in prose, so the
                        // token it was handed is what places the failure.
                        move |token| async move {
                            web.execute_websearch(prepared, token.clone())
                                .await
                                .map_err(|message| {
                                    let failure = if token.is_cancelled() {
                                        ToolFailure::Cancelled
                                    } else {
                                        ToolFailure::Other
                                    };
                                    ToolError::new(failure, message)
                                })
                        }
                    })
                    .await
                    .flatten()
                {
                    Ok(execution) => websearch_result(execution),
                    Err(error) => failed(error),
                }
            }
            (Input::Webfetch(_), PreparedExecution::Webfetch(prepared)) => {
                match self
                    .host
                    .run(ctx, {
                        let web = self.host.web.clone();
                        move |token| async move {
                            web.execute_webfetch(prepared, token)
                                .await
                                .map_err(webfetch_error)
                        }
                    })
                    .await
                    .flatten()
                {
                    Ok(execution) => webfetch_result(execution),
                    Err(error) => failed(error),
                }
            }
            (Input::Shell(_), PreparedExecution::Shell(group, prepared)) => {
                let progress = Arc::new(NativeProgressSink::new(ctx));
                progress.publish_live_buf(ctx);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group
                            .execute_prepared(*prepared, token, Some(progress))
                            .await
                    })
                    .await
                {
                    Ok(Ok(Some(execution))) => shell_result(execution),
                    Ok(Ok(None)) => ToolExecResult::failed(ToolFailure::Cancelled, SHELL_CANCELLED),
                    Ok(Err(error)) => ToolExecResult::failed(ToolFailure::Other, error),
                    Err(error) => failed(error),
                }
            }
            (Input::Code(input), PreparedExecution::None) => {
                let Some(code) = self.host.code.clone() else {
                    return ToolExecResult::failed(ToolFailure::Other, CODE_WORKER_UNAVAILABLE);
                };
                let input = input.clone();
                match self
                    .host
                    .run(
                        ctx,
                        move |token| async move { code.execute(input, token).await },
                    )
                    .await
                {
                    Ok(Ok(Some(execution))) => code_result(execution),
                    Ok(Ok(None)) => ToolExecResult::failed(ToolFailure::Cancelled, CODE_CANCELLED),
                    Ok(Err(error)) => failed(invalid_input(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::CodeMap(input), PreparedExecution::CodeGraph(group)) => {
                let input = input.clone();
                let sink = self.graph_progress(ctx);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.code_map(input, sink.as_deref(), &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        let output = fit(output);
                        let annotation = shown_of(output.shown, output.total, "symbols");
                        code_graph_result(
                            &output,
                            format!("ranked symbols in {}", output.path),
                            ranked_rows(&output.symbols),
                            None,
                            graph_footer(&output),
                            annotation,
                        )
                    }
                    Ok(Err(error)) => failed(code_graph_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::CodeContext(input), PreparedExecution::CodeGraph(group)) => {
                let input = input.clone();
                let sink = self.graph_progress(ctx);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.code_context(input, sink.as_deref(), &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        let annotation = format!(
                            "{} \u{b7} {} confidence",
                            shown_of(output.shown, output.total_matched, "matches"),
                            output.confidence
                        );
                        let headline = format!(
                            "read as {} ({}); confidence {} at {}% separation",
                            output.shape,
                            output.shape_reason,
                            output.confidence,
                            output.margin_percent
                        );
                        let output = fit(output);
                        code_graph_result(
                            &output,
                            headline,
                            ranked_rows(&output.results),
                            None,
                            graph_footer(&output),
                            annotation,
                        )
                    }
                    Ok(Err(error)) => failed(code_graph_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::CodeRefs(input), PreparedExecution::CodeGraph(group)) => {
                let input = input.clone();
                let sink = self.graph_progress(ctx);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.code_refs(input, sink.as_deref(), &token).await
                    })
                    .await
                {
                    Ok(Ok(Ok(output))) => {
                        let output = fit(output);
                        let annotation = shown_of(output.shown, output.total, output.unit);
                        code_graph_result(
                            &output,
                            format!(
                                "{} of {}, each row one {}",
                                output.direction, output.symbol, output.unit
                            ),
                            ranked_rows(&output.references),
                            None,
                            graph_footer(&output),
                            annotation,
                        )
                    }
                    Ok(Ok(Err(refusal))) => selector_refusal_result(&refusal),
                    Ok(Err(error)) => failed(code_graph_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::CodeImpact(input), PreparedExecution::CodeGraph(group)) => {
                let input = input.clone();
                let sink = self.graph_progress(ctx);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.code_impact(input, sink.as_deref(), &token).await
                    })
                    .await
                {
                    Ok(Ok(Ok(output))) => {
                        let output = fit(output);
                        let annotation = format!(
                            "{} \u{b7} {} tests",
                            shown_of(output.shown, output.total, "reached"),
                            output.tests_reaching.len()
                        );
                        code_graph_result(
                            &output,
                            format!(
                                "{} symbols reach {} within {} hops; {} of them are tests",
                                output.total,
                                output.symbol,
                                output.depth,
                                output.tests_reaching.len()
                            ),
                            reached_rows(&output.reached),
                            None,
                            graph_footer(&output),
                            annotation,
                        )
                    }
                    Ok(Ok(Err(refusal))) => selector_refusal_result(&refusal),
                    Ok(Err(error)) => failed(code_graph_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::CodeExpand(input), PreparedExecution::CodeGraph(group)) => {
                let input = input.clone();
                let sink = self.graph_progress(ctx);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.code_expand(input, sink.as_deref(), &token).await
                    })
                    .await
                {
                    Ok(Ok(Ok(output))) => {
                        let output = fit(output);
                        let annotation = format!(
                            "{} callers \u{b7} {} callees",
                            output.callers.len(),
                            output.callees.len()
                        );
                        let source = CodeGraphSource {
                            path: output.path.clone(),
                            kind: output.kind.clone(),
                            line_start: output.line_start,
                            lines: output.source.lines().map(str::to_owned).collect(),
                            whole_file_reason: output.served_whole_file.clone(),
                        };
                        code_graph_result(
                            &output,
                            format!(
                                "{} {} {}:{}-{}",
                                output.symbol,
                                output.kind,
                                output.path,
                                output.line_start,
                                output.line_end
                            ),
                            neighbour_rows(&output.callers, &output.callees),
                            Some(source),
                            graph_footer(&output),
                            annotation,
                        )
                    }
                    Ok(Ok(Err(refusal))) => selector_refusal_result(&refusal),
                    Ok(Err(error)) => failed(code_graph_error(error)),
                    Err(error) => failed(error),
                }
            }
            (Input::Environment, PreparedExecution::Environment(environment)) => {
                let groups = ToolGroupDisclosure {
                    files: true,
                    web: true,
                    shell: true,
                    code: self.host.code.is_some(),
                    code_graph: true,
                };
                match self
                    .host
                    .run(ctx, move |token| async move {
                        environment.inspect(groups, token).await
                    })
                    .await
                {
                    Ok(Ok(result)) => environment_result(result),
                    Ok(Err(error)) => failed(environment_error(error)),
                    Err(error) => failed(error),
                }
            }
            _ => Err("Workcell invocation preparation did not match its typed input".into()).into(),
        }
    }
}

fn stale_notice(ctx: &ToolContext, paths: &[PathBuf]) -> Option<String> {
    if !ctx.config.stale_read_check {
        return None;
    }
    paths
        .iter()
        .find_map(|path| ctx.file_tracker.check_before_edit(path).err())
}

fn with_stale_notice(error: String, notice: Option<String>) -> String {
    match notice {
        Some(notice) => format!("{error}\n\n{notice}"),
        None => error,
    }
}

fn failed(error: ToolError) -> ToolExecResult {
    ToolExecResult::failed(error.failure, error.message)
}

fn invalid_input(message: String) -> ToolError {
    ToolError::new(ToolFailure::InvalidInput, message)
}

/// Workcell's own symbolic code, the one a remote host refuses with, so a
/// local and a remote failure land in the same bucket.
fn filesystem_error(error: FilesystemError) -> ToolError {
    ToolError::new(ToolFailure::from_code(error.code()), error.to_string())
}

fn webfetch_error(error: WebfetchError) -> ToolError {
    ToolError::new(ToolFailure::from_code(error.code()), error.to_string())
}

fn shell_preparation_error(error: ShellPreparationError) -> ToolError {
    ToolError::new(ToolFailure::from_code(error.code()), error.to_string())
}

fn code_graph_error(error: CodeGraphError) -> ToolError {
    let failure = match &error {
        CodeGraphError::Denied(_) => ToolFailure::Denied,
        CodeGraphError::Invalid(_) => ToolFailure::InvalidInput,
        CodeGraphError::Aborted => ToolFailure::Cancelled,
        CodeGraphError::Internal(_) => ToolFailure::Other,
    };
    ToolError::new(failure, error.to_string())
}

fn workspace_error(error: WorkspaceError) -> ToolError {
    ToolError::new(ToolFailure::from(&error), error.to_string())
}

async fn confined_traversal_group(
    unconfined: FileToolGroup,
    resource: &FileResource,
) -> Result<(FileToolGroup, String), FilesystemError> {
    let is_directory = tokio::fs::metadata(&resource.path)
        .await
        .is_ok_and(|metadata| metadata.is_dir());
    if !is_directory {
        return Ok((unconfined, resource.path.to_string_lossy().into_owned()));
    }
    let limits = *unconfined.limits();
    let confined = FileToolGroup::new(&resource.path, false, Some(limits)).await?;
    Ok((confined, ".".into()))
}

/// A read of a path that is not there can only fail, so the call is refused here
/// rather than raised as a permission prompt. Nothing is authorized: the model
/// gets the same "no such file" it would have got after an approval, without
/// spending a user interaction on it. Refusing outright rather than allowing
/// silently also leaves no window for the path to appear between this check and
/// the read. Writes are exempt, because a write to a missing path creates it,
/// which is precisely the case worth confirming.
fn missing_read_target(intent: &PermissionIntent) -> Option<&str> {
    intent.resources.iter().find_map(|resource| {
        let reads = matches!(
            resource.access,
            Some(
                PermissionResourceAccess::Read
                    | PermissionResourceAccess::Search
                    | PermissionResourceAccess::List
            )
        );
        let filesystem = matches!(
            resource.kind,
            PermissionResourceKind::File | PermissionResourceKind::Directory
        );
        (reads && filesystem && !Path::new(&resource.value).exists())
            .then_some(resource.value.as_str())
    })
}

fn file_read_prepared(
    project: &Path,
    group: FileToolGroup,
    read: PreparedFileRead,
) -> PreparedInvocation {
    let directory = read.resource().access == FileResourceAccess::Traverse;
    let mut resource = filesystem_permission_resource(
        if directory {
            PermissionResourceKind::Directory
        } else {
            PermissionResourceKind::File
        },
        &read.resource().path,
        if directory {
            PermissionResourceAccess::List
        } else {
            PermissionResourceAccess::Read
        },
        project,
    );
    if directory {
        resource
            .attributes
            .insert(BROWSE_RECURSION_ATTRIBUTE.into(), BROWSE_DIRECT.into());
    }
    let read_targets = if directory {
        Vec::new()
    } else {
        vec![read.resource().path.clone()]
    };
    PreparedInvocation {
        intent: PermissionIntent::new(
            PermissionScopes::single(resource.value.clone()),
            vec![resource],
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        }),
        execution: if directory {
            PreparedExecution::DirectoryRead(group, read)
        } else {
            PreparedExecution::FileRead(group, read)
        },
        mutation_targets: Vec::new(),
        read_targets,
    }
}

fn file_prepared(
    resources: Vec<FileResource>,
    project: &Path,
    group: FileToolGroup,
    input: Input,
    input_pointers: &[&str],
) -> PreparedInvocation {
    let mut permissions = file_permissions(&resources, project);
    if matches!(input, Input::FileGlob(_)) {
        for resource in &mut permissions.resources {
            resource.access = Some(PermissionResourceAccess::List);
            resource
                .attributes
                .insert(BROWSE_RECURSION_ATTRIBUTE.into(), BROWSE_RECURSIVE.into());
        }
    }
    let mutation = !permissions.mutation_targets.is_empty();
    PreparedInvocation {
        intent: PermissionIntent::new(
            PermissionScopes {
                scopes: permissions.scopes,
                force_prompt: false,
                plan_scoped: false,
            },
            permissions.resources,
            if mutation {
                PermissionRisk::High
            } else {
                PermissionRisk::Low
            },
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: input_pointers
                .iter()
                .map(|pointer| (*pointer).into())
                .collect(),
        }),
        execution: PreparedExecution::File(group, input),
        mutation_targets: permissions.mutation_targets,
        read_targets: permissions.read_targets,
    }
}

fn index_prepared(
    resource: FileResource,
    project: &Path,
    group: FileToolGroup,
) -> PreparedInvocation {
    let permissions = file_permissions(std::slice::from_ref(&resource), project);
    PreparedInvocation {
        intent: PermissionIntent::new(
            PermissionScopes {
                scopes: permissions.scopes,
                force_prompt: false,
                plan_scoped: false,
            },
            permissions.resources,
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        }),
        execution: PreparedExecution::Index(group, resource),
        mutation_targets: Vec::new(),
        read_targets: permissions.read_targets,
    }
}

fn file_patch_prepared(
    resources: Vec<FileResource>,
    project: &Path,
    group: FileToolGroup,
    patch: PreparedFilePatch,
) -> PreparedInvocation {
    let permissions = file_permissions(&resources, project);
    PreparedInvocation {
        intent: PermissionIntent::new(
            PermissionScopes {
                scopes: permissions.scopes,
                force_prompt: false,
                plan_scoped: false,
            },
            permissions.resources,
            if permissions.mutation_targets.is_empty() {
                PermissionRisk::Low
            } else {
                PermissionRisk::High
            },
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        }),
        execution: PreparedExecution::FilePatch(group, patch),
        mutation_targets: permissions.mutation_targets,
        read_targets: permissions.read_targets,
    }
}

/// One read-only intent over the tree a code-graph call will crawl.
///
/// The scope is the directory, never the symbol: the call reads every source
/// file underneath it to build the graph, and a narrower scope would claim an
/// access this tool does not have. `read_targets` stays empty for the same
/// reason `file_grep`'s does — a crawl cannot name the files it will read
/// before it runs.
fn code_graph_prepared(
    group: Arc<CodeGraphToolGroup>,
    project: &Path,
    path: Option<&str>,
    input_pointers: &[&str],
) -> PreparedInvocation {
    let root = match path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => project.join(path),
        None => project.to_path_buf(),
    };
    let scope = format!(
        "{}/**",
        root.to_string_lossy().trim_end_matches(['/', '\\'])
    );
    PreparedInvocation {
        intent: PermissionIntent::new(
            PermissionScopes {
                scopes: vec![scope],
                force_prompt: false,
                plan_scoped: false,
            },
            vec![filesystem_permission_resource(
                PermissionResourceKind::Directory,
                &root,
                PermissionResourceAccess::Search,
                project,
            )],
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: input_pointers.iter().map(|p| (*p).to_owned()).collect(),
        }),
        execution: PreparedExecution::CodeGraph(group),
        mutation_targets: Vec::new(),
        read_targets: Vec::new(),
    }
}

struct FilePermissions {
    scopes: Vec<String>,
    resources: Vec<PermissionResource>,
    mutation_targets: Vec<PathBuf>,
    read_targets: Vec<PathBuf>,
}

fn file_permissions(resources: &[FileResource], project: &Path) -> FilePermissions {
    let mut scopes = Vec::with_capacity(resources.len());
    let mut permission_resources = Vec::with_capacity(resources.len());
    let mut mutation_targets = Vec::new();
    let mut read_targets = Vec::new();
    for resource in resources {
        // `whole_file_read` is decided here rather than from `kind` below,
        // which the permission resource takes ownership of.
        let (kind, access, mutation, whole_file_read) = match resource.access {
            FileResourceAccess::Read if resource.path.is_dir() => (
                PermissionResourceKind::Directory,
                PermissionResourceAccess::Read,
                false,
                false,
            ),
            FileResourceAccess::Read => (
                PermissionResourceKind::File,
                PermissionResourceAccess::Read,
                false,
                true,
            ),
            FileResourceAccess::Traverse => (
                PermissionResourceKind::Directory,
                PermissionResourceAccess::Search,
                false,
                false,
            ),
            FileResourceAccess::Write
            | FileResourceAccess::ReadWrite
            | FileResourceAccess::Delete => (
                PermissionResourceKind::File,
                PermissionResourceAccess::Write,
                true,
                false,
            ),
        };
        let value = resource.path.to_string_lossy().into_owned();
        scopes.push(if kind == PermissionResourceKind::Directory {
            format!("{}/**", value.trim_end_matches(['/', '\\']))
        } else {
            value
        });
        permission_resources.push(filesystem_permission_resource(
            kind,
            &resource.path,
            access,
            project,
        ));
        if mutation {
            mutation_targets.push(resource.path.clone());
        } else if whole_file_read {
            // Whole-file reads only. A directory is too coarse to lock, and a
            // search reports matches this call cannot name in advance.
            read_targets.push(resource.path.clone());
        }
    }
    mutation_targets.sort();
    mutation_targets.dedup();
    read_targets.sort();
    read_targets.dedup();
    FilePermissions {
        scopes,
        resources: permission_resources,
        mutation_targets,
        read_targets,
    }
}

fn shell_prepared(
    group: ShellToolGroup,
    shell: PreparedShell,
    project: &Path,
    raw_input: Option<&Value>,
    redirect: ShellNativeRedirect,
    workdir_redirect: bool,
) -> Result<PreparedInvocation, ToolError> {
    let raw_input = raw_input
        .filter(|input| input.get("command").and_then(Value::as_str) == Some(shell.command()));
    let mut opacity = Some(ShellOpacity::Unparsed);
    let mut resources = Vec::new();
    let mut scopes = Vec::new();
    if let Ok(program) = shell.bash_program() {
        if workdir_redirect && native_redirect::leading_workdir(program) {
            tracing::info!(
                argument = "workdir",
                "shell command uses a leading cd instead of workdir"
            );
            return Err(ToolError::new(
                ToolFailure::Denied,
                native_redirect::WORKDIR_REFUSAL,
            ));
        }
        let contexts = shell
            .bash_command_contexts()
            .unwrap_or_else(|_| program.command_contexts(shell.workdir()));
        let facts = pattern_analysis::shell_facts(program, &contexts);
        opacity = facts.opacity;
        if !facts.opaque
            && redirect != ShellNativeRedirect::Off
            && let Some(natives) = native_redirect::detect(&facts.commands)
        {
            let enforced = redirect == ShellNativeRedirect::Enforce;
            tracing::info!(
                tools = natives
                    .iter()
                    .map(|native| native.name)
                    .collect::<Vec<_>>()
                    .join(","),
                commands = facts.commands.len(),
                enforced,
                "shell command duplicates a native tool"
            );
            if enforced {
                return Err(ToolError::new(
                    ToolFailure::Denied,
                    native_redirect::refusal(&natives),
                ));
            }
        }
        for command in &facts.commands {
            let workdir = pattern_analysis::singleton_workdir(command);
            let mut attributes = BTreeMap::new();
            if let Some(workdir) = workdir {
                attributes.insert("workdir".into(), workdir.to_string_lossy().into_owned());
            }
            if let Some(context) = command.context
                && let Ok(incoming) = serde_json::to_string(&context.incoming)
            {
                attributes.insert("possible_workdirs".into(), incoming);
            }
            attributes.insert(
                NORMALIZED_COMMAND_ATTRIBUTE.into(),
                command.scope.normalized.clone(),
            );
            if opacity.is_none()
                && command.context.is_some_and(|context| {
                    read_only_shell::confined_read(&command.scope, &context.incoming, project)
                })
            {
                attributes.insert(CONFINED_READ_ATTRIBUTE.into(), CONFINED_READ_VALUE.into());
            }
            if !facts.opaque
                && let Some(raw_input) = raw_input
                && let Some(mut observation) = pattern_analysis::command_observation(
                    program,
                    command,
                    shell.workdir(),
                    project,
                    ObservationProvenance::Native,
                )
            {
                let binding = prepared_command_binding(&command.scope.source, raw_input);
                observation.source.input_hash = binding.clone();
                if let Ok(observation) = serde_json::to_string(&observation) {
                    attributes.insert(COMMAND_OBSERVATION_ATTRIBUTE.into(), observation);
                    attributes.insert(COMMAND_OBSERVATION_BINDING_ATTRIBUTE.into(), binding);
                }
            }
            scopes.push(shell_permission_scope(
                &command.scope.source,
                workdir.unwrap_or(shell.workdir()),
            ));
            resources.push(PermissionResource {
                kind: PermissionResourceKind::Command,
                value: command.scope.source.clone(),
                access: Some(PermissionResourceAccess::Execute),
                protected: false,
                requires_prompt: false,
                attributes,
            });
        }
    }
    if let Some(opacity) = opacity {
        scopes.push(shell_permission_scope(shell.command(), shell.workdir()));
        resources.push(PermissionResource {
            kind: PermissionResourceKind::Command,
            value: shell.command().into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: true,
            requires_prompt: true,
            attributes: BTreeMap::from([
                (
                    "workdir".into(),
                    shell.workdir().to_string_lossy().into_owned(),
                ),
                (OPACITY_ATTRIBUTE.into(), opacity.to_string()),
            ]),
        });
    }
    Ok(PreparedInvocation {
        intent: PermissionIntent::new(
            // Opaque commands carry `requires_prompt` instead of forcing a prompt on
            // the whole request: scope allows and configured command allows still
            // cannot cover them, while an explicitly confirmed structured authority
            // can.
            PermissionScopes {
                scopes,
                force_prompt: false,
                plan_scoped: false,
            },
            resources,
            if opacity.is_some() {
                PermissionRisk::Critical
            } else {
                PermissionRisk::High
            },
        )
        .with_authority(PermissionAuthorityProfile::Shell),
        execution: PreparedExecution::Shell(group, Box::new(shell)),
        // A command's writes are not knowable from its text, so shell neither
        // takes guards nor invalidates the tracker.
        mutation_targets: Vec::new(),
        read_targets: Vec::new(),
    })
}

fn exact_custom_prepared(
    name: &str,
    value: &str,
    access: PermissionResourceAccess,
    risk: PermissionRisk,
) -> PreparedInvocation {
    PreparedInvocation {
        intent: PermissionIntent::new(
            PermissionScopes::single(value.into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom { name: name.into() },
                value: value.into(),
                access: Some(access),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            risk,
        ),
        execution: PreparedExecution::None,
        mutation_targets: Vec::new(),
        read_targets: Vec::new(),
    }
}

fn text_output(text: String, state: Value) -> TextOutput {
    TextOutput {
        text,
        instructions: None,
        state: Some(state),
        lua_provenance: None,
    }
}

fn text_result(
    output: &impl Serialize,
    text: String,
    markdown: bool,
    exact_model_text: String,
) -> ToolExecResult {
    let state = serde_json::to_value(output).expect("Workcell structured output serializes");
    let output = if markdown {
        ToolOutput::Markdown(text_output(text, state))
    } else {
        ToolOutput::Plain(text_output(text, state))
    };
    ToolExecResult::from(Ok::<_, String>(output)).with_model_output(Some(exact_model_text))
}

/// The record is what the card draws from; the model reads the rendering.
/// `ReadCode` numbers its lines and names the offset that continues the file,
/// which is the form the tool documents and the form a batched read has always
/// returned. Serializing the record instead sent the text twice, once escaped
/// into a JSON string, and dropped the line numbers on the way.
fn file_read_result(output: FileReadOutput) -> ToolExecResult {
    let state = serde_json::to_value(&output).expect("file read output serializes");
    let tool_output = match output {
        FileReadOutput::Directory {
            entries, truncated, ..
        } => {
            let mut listing = entries.join("\n");
            // The record carried a flag. A listing has to say it in words, or a
            // capped directory reads as a complete one.
            if truncated {
                listing.push('\n');
                listing.push_str(INDEX_TRUNCATED);
            }
            ToolOutput::ReadDir(text_output(listing, state))
        }
        FileReadOutput::File {
            path,
            text,
            line_start,
            total_lines,
            ..
        } => ToolOutput::ReadCode {
            path,
            start_line: line_start,
            lines: text.split('\n').map(str::to_owned).collect(),
            total_lines,
            instructions: None,
        },
    };
    ToolExecResult::from(Ok::<_, String>(tool_output))
}

/// A search reports its own truncation notice, so the rendering Workcell writes
/// is what both the model and the reader get. Serializing the record instead
/// would carry every path twice and drop the sentence that says what was
/// withheld.
fn file_glob_result(output: FileGlobOutput) -> ToolExecResult {
    let text = output.model_text().into_owned();
    let display = if text.is_empty() {
        caudra_agent::NO_FILES_FOUND.into()
    } else {
        text.clone()
    };
    let annotation = glob_annotation(&output);
    text_result(&output, display, false, text).with_annotation(Some(annotation))
}

/// `count` is the window, `total` what matched. A scan that stopped early knows
/// neither exactly, so it says so rather than quoting a total as if it were one.
fn glob_annotation(output: &FileGlobOutput) -> String {
    let files = if output.count == 1 { "file" } else { "files" };
    if !output.truncated {
        return format!("{} {files}", output.count);
    }
    if output.scan_complete {
        return format!("{} of {} files", output.count, output.total);
    }
    if output.total > output.count {
        return format!("{} of at least {} files", output.count, output.total);
    }
    format!("{} {files}, scan capped", output.count)
}

fn file_grep_result(output: FileGrepOutput) -> ToolExecResult {
    let exact = output.model_text().into_owned();
    let capped = output.truncated.then_some(SearchCap {
        files_scanned: output.files_scanned,
        files_listed: output.files_listed,
    });
    let mut entries: Vec<GrepFileEntry> = Vec::new();
    for row in output.rows {
        let group = GrepMatchGroup::single(row.line, row.text);
        if let Some(entry) = entries
            .last_mut()
            .filter(|entry| entry.path == row.relative_path)
        {
            entry.groups.push(group);
        } else {
            entries.push(GrepFileEntry {
                path: row.relative_path,
                groups: vec![group],
            });
        }
    }
    ToolExecResult::from(Ok::<_, String>(ToolOutput::GrepResult { entries, capped }))
        .with_model_output(Some(exact))
}

fn index_result(output: WorkcellIndexOutput, max_model_output_bytes: usize) -> ToolExecResult {
    let state = serde_json::to_value(&output).expect("index output serializes");
    let (output, model_output) = match output {
        WorkcellIndexOutput::File {
            path,
            relative_path,
            language,
            skeleton,
            lines,
            source_line_count,
            parse_error,
            truncated,
        } => {
            let model_output = skeleton.clone();
            (
                AgentIndexOutput::File {
                    path,
                    relative_path,
                    language,
                    skeleton,
                    lines: lines
                        .into_iter()
                        .map(|line| AgentIndexLine {
                            output_line: line.output_line,
                            text: line.text,
                            semantic: match line.semantic {
                                IndexLineSemantic::Section => AgentIndexLineSemantic::Section,
                                IndexLineSemantic::Item => AgentIndexLineSemantic::Item,
                                IndexLineSemantic::Dimmed => AgentIndexLineSemantic::Dimmed,
                                IndexLineSemantic::Plain => AgentIndexLineSemantic::Plain,
                            },
                            body: line.body,
                            source_range: line.source_range.map(|range| AgentIndexSourceRange {
                                start_line: range.start_line,
                                end_line: range.end_line,
                            }),
                        })
                        .collect(),
                    source_line_count,
                    parse_error,
                    truncated,
                    instructions: None,
                    state: Some(state),
                },
                model_output,
            )
        }
        WorkcellIndexOutput::Directory {
            path,
            relative_path,
            entries,
            total_count,
            truncated,
            listing,
        } => {
            let listing =
                directory_listing_with_truncation(&listing, truncated, max_model_output_bytes);
            let model_output = listing.clone();
            (
                AgentIndexOutput::Directory {
                    path,
                    relative_path,
                    entries: entries
                        .into_iter()
                        .map(|entry| AgentIndexDirectoryEntry {
                            name: entry.name,
                            kind: match entry.kind {
                                IndexDirectoryEntryKind::Directory => {
                                    AgentIndexDirectoryEntryKind::Directory
                                }
                                IndexDirectoryEntryKind::File => AgentIndexDirectoryEntryKind::File,
                            },
                        })
                        .collect(),
                    total_count,
                    truncated,
                    listing,
                    instructions: None,
                    state: Some(state),
                },
                model_output,
            )
        }
    };
    ToolExecResult::from(Ok::<_, String>(ToolOutput::Index(output)))
        .with_model_output(Some(model_output))
}

fn directory_listing_with_truncation(listing: &str, truncated: bool, max_bytes: usize) -> String {
    if !truncated || listing.lines().last() == Some(INDEX_TRUNCATED) {
        return listing.to_owned();
    }
    if listing.is_empty() {
        return INDEX_TRUNCATED.to_owned();
    }

    let suffix = format!("\n{INDEX_TRUNCATED}");
    let budget = max_bytes.saturating_sub(suffix.len());
    if listing.len() <= budget {
        return format!("{listing}{suffix}");
    }
    let mut end = budget.min(listing.len());
    while !listing.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let prefix = &listing[..end];
    let prefix = prefix
        .rfind('\n')
        .map_or("", |line_end| &prefix[..line_end]);
    if prefix.is_empty() {
        INDEX_TRUNCATED.to_owned()
    } else {
        format!("{prefix}{suffix}")
    }
}

/// A write reports what it did, not what it wrote: the content is the model's
/// own, and echoing it back as an escaped patch inside a JSON wrapper charged
/// for it twice.
///
/// What it did depends on what was there. A create is its content, because a
/// new file has no other side and a diff of one is every line with a `+`. An
/// overwrite is a change, and is shown as one whenever Workcell could carry the
/// content it replaced within `maxPreviousBytes`. Above that bound, and when
/// nothing was written at all, the patch is the whole report.
fn file_write_result(output: FileWriteOutput, content: String) -> ToolExecResult {
    let FileWriteOutput {
        path,
        existed,
        applied,
        diff,
        previous,
        ..
    } = output;
    let written = applied.then(|| path.clone());
    let result = match (applied, existed, previous) {
        (true, false, _) => ToolOutput::WriteCode {
            path,
            byte_count: content.len(),
            lines: content.lines().map(str::to_owned).collect(),
        },
        (true, true, Some(before)) => ToolOutput::Diff {
            path,
            before,
            after: content,
            summary: diff.patch,
        },
        // Either nothing was written, or the old side was too large to carry.
        // A diff alone reads as a write that landed, so a result that did not
        // land says so below.
        _ => ToolOutput::Patch {
            files: vec![patched_file(&diff)],
        },
    };
    let result = ToolExecResult::from(Ok::<_, String>(result));
    let result = if applied {
        result
    } else {
        result.with_model_suffix(Some(NOT_APPLIED.to_owned()))
    };
    result.with_written_paths(written.into_iter().collect())
}

fn file_edit_result(
    output: FileEditOutput,
    old_string: String,
    new_string: String,
    replace_all: bool,
) -> ToolExecResult {
    let written = output.applied.then(|| output.path.clone());
    let result = if replace_all {
        // Every match moved at once, so there is no single before/after pair
        // to diff. The patch carries all of them with their real line numbers.
        ToolExecResult::from(Ok::<_, String>(ToolOutput::Patch {
            files: vec![patched_file(&output.diff)],
        }))
    } else {
        ToolExecResult::from(Ok::<_, String>(ToolOutput::Diff {
            path: output.path.clone(),
            before: old_string,
            after: new_string,
            summary: output.diff.patch.clone(),
        }))
    };
    let result = if output.applied {
        result
    } else {
        result.with_model_suffix(Some(NOT_APPLIED.to_owned()))
    };
    result.with_written_paths(written.into_iter().collect())
}

fn patched_file(diff: &FileDiff) -> PatchedFile {
    PatchedFile {
        path: diff.relative_path.clone(),
        patch: diff.patch.clone(),
        additions: diff.additions,
        deletions: diff.deletions,
        truncated: diff.truncated,
    }
}

fn applied_patch_paths(output: &FileApplyPatchOutput) -> Vec<String> {
    if !output.applied {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for file in &output.files {
        paths.push(file.file_path.clone());
        if let Some(path) = &file.move_path {
            paths.push(path.clone());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn file_patch_result(output: FileApplyPatchOutput) -> ToolExecResult {
    let written = applied_patch_paths(&output);
    let files = output
        .files
        .iter()
        .map(|file| PatchedFile {
            path: file.relative_path.clone(),
            patch: file.patch.clone(),
            additions: file.additions,
            deletions: file.deletions,
            truncated: file.truncated,
        })
        .collect();
    let result = ToolExecResult::from(Ok::<_, String>(ToolOutput::Patch { files }));
    let result = if output.applied {
        result
    } else {
        result.with_model_suffix(Some(NOT_APPLIED.to_owned()))
    };
    result.with_written_paths(written)
}

fn websearch_result(execution: WebExecution<WebsearchOutput>) -> ToolExecResult {
    text_result(
        &execution.output,
        execution.model_text.clone(),
        true,
        execution.model_text,
    )
}

fn webfetch_result(execution: WebExecution<WebfetchOutput>) -> ToolExecResult {
    let markdown = matches!(
        execution.output.format,
        workcell::web::WebfetchFormat::Markdown
    );
    text_result(
        &execution.output,
        execution.model_text.clone(),
        markdown,
        execution.model_text,
    )
}

fn shell_result(execution: ShellExecution) -> ToolExecResult {
    shell_result_parts(execution.output, execution.model_text, execution.filter)
}

fn shell_result_parts(
    output: WorkcellShellOutput,
    mut model_text: String,
    filter: Option<WorkcellShellFilterInfo>,
) -> ToolExecResult {
    model_text.push_str("\n\n");
    model_text.push_str(&shell_status(&output));
    let output = AgentShellOutput {
        model_text: model_text.clone(),
        relative_workdir: output.relative_workdir,
        timeout_ms: output.timeout_ms,
        duration_ms: output.duration_ms,
        exit_code: output.exit_code,
        signal: output.signal,
        timed_out: output.timed_out,
        output_limit_exceeded: output.output_limit_exceeded,
        final_sequence: output.final_sequence,
        stdout_utf8_bytes: output.stdout_utf8_bytes,
        stderr_utf8_bytes: output.stderr_utf8_bytes,
        stdout: output.stdout,
        stderr: output.stderr,
        stdout_capture_truncated: output.stdout_capture_truncated,
        stderr_capture_truncated: output.stderr_capture_truncated,
        stdout_preview_truncated: output.stdout_preview_truncated,
        stderr_preview_truncated: output.stderr_preview_truncated,
        stdout_redraws_collapsed: output.stdout_redraws_collapsed,
        stderr_redraws_collapsed: output.stderr_redraws_collapsed,
        filter: filter.map(|filter| AgentShellFilterInfo {
            stages: filter.stages,
            unfiltered_utf8_bytes: filter.unfiltered_utf8_bytes,
            filtered_utf8_bytes: filter.filtered_utf8_bytes,
        }),
    };
    shell_exec_result(output, model_text)
}

/// A command that ran and failed on its own states no reason. Only Workcell's
/// own time limit makes it a timeout, whatever the command printed.
fn shell_exec_result(output: AgentShellOutput, model_output: String) -> ToolExecResult {
    let timed_out = output.timed_out;
    let is_error = output.exit_code != Some(0) || timed_out || output.output_limit_exceeded;
    let result = ToolExecResult::from(Ok::<_, String>(ToolOutput::Shell(output)))
        .with_model_output(Some(model_output))
        .with_error(is_error);
    if timed_out {
        result.with_failure(ToolFailure::Timeout)
    } else {
        result
    }
}

fn shell_status(output: &WorkcellShellOutput) -> String {
    let mut statuses = Vec::new();
    if output.timed_out {
        statuses.push("timed out".into());
    }
    if output.output_limit_exceeded {
        statuses.push("output limit exceeded".into());
    }
    if let Some(exit_code) = output.exit_code {
        statuses.push(format!("exit code {exit_code}"));
    } else if let Some(signal) = output.signal {
        statuses.push(format!("signal {signal}"));
    } else if statuses.is_empty() {
        statuses.push("exit unknown".into());
    }
    format!("[shell status: {}]", statuses.join("; "))
}

/// Turns one code-graph answer into a card.
///
/// The model text is Workcell's own `ModelText`, never a restatement: that
/// rendering carries the floor caveats and the truncation notice, and a second
/// one here would be a second contract to keep in step.
fn code_graph_result(
    output: &(impl Serialize + CodeGraphModelText),
    headline: String,
    rows: Vec<CodeGraphRow>,
    source: Option<CodeGraphSource>,
    footer: String,
    annotation: String,
) -> ToolExecResult {
    let state = serde_json::to_value(output).expect("code graph output serializes");
    let exact = output.model_text().into_owned();
    ToolExecResult::from(Ok::<_, String>(ToolOutput::CodeGraph {
        headline,
        rows,
        source,
        footer,
        state: Some(state),
    }))
    .with_model_output(Some(exact))
    .with_annotation(Some(annotation))
}

/// A refused selector is a successful call: the did-you-mean list is the useful
/// answer, and an error envelope has nowhere to put it.
fn selector_refusal_result(refusal: &SelectorRefusal) -> ToolExecResult {
    let exact = refusal.model_text().into_owned();
    let state = serde_json::to_value(refusal).expect("refusal serializes");
    let candidates = refusal.did_you_mean.len();
    ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain(text_output(
        exact.clone(),
        state,
    ))))
    .with_model_output(Some(exact))
    .with_annotation(Some(if candidates == 0 {
        "no match".to_owned()
    } else {
        format!("no match \u{b7} {candidates} candidates")
    }))
}

fn ranked_rows(symbols: &[RankedSymbol]) -> Vec<CodeGraphRow> {
    symbols
        .iter()
        .map(|symbol| CodeGraphRow {
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            path: symbol.path.clone(),
            line_start: symbol.line_start,
            line_end: symbol.line_end,
            inbound: Some(symbol.callers),
            outbound: Some(symbol.calls),
            hops: None,
            test_scope: symbol.test_scope,
        })
        .collect()
}

fn reached_rows(reached: &[ReachedSymbol]) -> Vec<CodeGraphRow> {
    reached
        .iter()
        .map(|row| CodeGraphRow {
            name: row.symbol.name.clone(),
            kind: row.symbol.kind.clone(),
            path: row.symbol.path.clone(),
            line_start: row.symbol.line_start,
            line_end: row.symbol.line_end,
            inbound: None,
            outbound: None,
            hops: Some(row.hops),
            test_scope: row.test_scope,
        })
        .collect()
}

fn neighbour_rows(callers: &[SymbolRef], callees: &[SymbolRef]) -> Vec<CodeGraphRow> {
    callers
        .iter()
        .map(|symbol| (symbol, Some(1_usize), None))
        .chain(callees.iter().map(|symbol| (symbol, None, Some(1_usize))))
        .map(|(symbol, inbound, outbound)| CodeGraphRow {
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            path: symbol.path.clone(),
            line_start: symbol.line_start,
            line_end: symbol.line_end,
            inbound,
            outbound,
            hops: None,
            test_scope: false,
        })
        .collect()
}

/// The trailing summary each result carries, taken from Workcell's own
/// rendering so the card and the model see the same caveats.
fn graph_footer(output: &impl CodeGraphModelText) -> String {
    output
        .model_text()
        .lines()
        .next_back()
        .unwrap_or_default()
        .to_owned()
}

fn shown_of(shown: usize, total: usize, unit: &str) -> String {
    if total > shown {
        format!("{shown} of {total} {unit}")
    } else {
        format!("{shown} {unit}")
    }
}

fn code_result(execution: CodeExecution) -> ToolExecResult {
    let (outcome, timed_out) = (execution.output.outcome, execution.output.timed_out);
    let result = text_result(
        &execution.output,
        execution.model_text.clone(),
        false,
        execution.model_text,
    );
    code_outcome_result(result, outcome, timed_out)
}

/// A snippet's failure is placed by the outcome Workcell typed. A spent budget
/// is a timeout only when time was the budget.
fn code_outcome_result(
    result: ToolExecResult,
    outcome: Outcome,
    timed_out: bool,
) -> ToolExecResult {
    let failure = match outcome {
        Outcome::Completed => return result,
        Outcome::Rejected => ToolFailure::InvalidInput,
        Outcome::Limited if timed_out => ToolFailure::Timeout,
        Outcome::Exception | Outcome::Limited | Outcome::Unavailable => ToolFailure::Other,
    };
    result.with_failure(failure)
}

/// The host as a card rather than as its record. Workcell's `model_text` is the
/// same descriptor pretty-printed, which costs the model 168 lines to say what
/// fourteen say, so the mapping renders the descriptor once and both the card
/// and the model read that.
fn environment_result(result: ExecutionEnvironmentResult) -> ToolExecResult {
    serde_json::to_value(result.output)
        .and_then(|value| environment_card(&value))
        .map_err(|error| format!("invalid Workcell environment result: {error}"))
        .into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentPresentation {
    scope: String,
    os: EnvironmentOs,
    runtime: EnvironmentRuntime,
    execution: EnvironmentExecution,
    container: EnvironmentContainer,
    workspace: EnvironmentWorkspace,
    tool_groups: EnvironmentGroups,
    commands: Vec<EnvironmentCommand>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentOs {
    family: String,
    architecture: String,
    kernel_release: Option<String>,
    distribution: Option<String>,
    wsl: bool,
    system_package_manager: EnvironmentSystemPackageManager,
}

#[derive(Deserialize)]
struct EnvironmentRuntime {
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentExecution {
    shell: String,
    sandbox: String,
    network_access: String,
    environment_inheritance: String,
    privilege: EnvironmentPrivilege,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentPrivilege {
    effective_root: Option<bool>,
    non_interactive_sudo: String,
}

#[derive(Deserialize)]
struct EnvironmentContainer {
    kind: String,
    evidence: Vec<String>,
}

#[derive(Deserialize)]
struct EnvironmentSystemPackageManager {
    name: String,
    available: bool,
    executable: Option<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentWorkspace {
    git: EnvironmentGit,
    package_manager: EnvironmentPackageManager,
}

#[derive(Deserialize)]
struct EnvironmentGit {
    repository: String,
}

#[derive(Deserialize)]
struct EnvironmentPackageManager {
    declared: Option<EnvironmentDeclaredPackageManager>,
    inferred: Option<String>,
    lockfiles: Vec<String>,
}

#[derive(Deserialize)]
struct EnvironmentDeclaredPackageManager {
    name: String,
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentGroups {
    files: bool,
    web: bool,
    shell: bool,
    code: bool,
    code_graph: bool,
}

fn environment_card(value: &Value) -> Result<ToolOutput, serde_json::Error> {
    let output = EnvironmentPresentation::deserialize(value)?;
    let headline = environment_headline(&output.os);
    let summary = environment_summary(&output.execution);
    let facts = environment_facts(&output);
    Ok(ToolOutput::Environment {
        headline,
        summary,
        facts,
        commands: output.commands,
    })
}

/// `ubuntu 24.04 · linux/x86_64 · kernel 6.18.5`. The distribution leads when
/// there is one, because it is the name a reader recognises the host by.
fn environment_headline(os: &EnvironmentOs) -> String {
    let mut parts = Vec::new();
    if let Some(distribution) = &os.distribution {
        parts.push(distribution.clone());
    }
    parts.push(format!("{}/{}", os.family, os.architecture));
    if let Some(kernel) = &os.kernel_release {
        parts.push(format!("kernel {kernel}"));
    }
    if os.wsl {
        parts.push("wsl".to_owned());
    }
    parts.join(ENVIRONMENT_SEPARATOR)
}

/// What a command runs in, what it can reach, and what it may do. The second
/// line of the card, and the last one a collapsed card still shows.
fn environment_summary(execution: &EnvironmentExecution) -> String {
    let mut parts = vec![
        execution.shell.to_owned(),
        format!("{} sandbox", execution.sandbox),
        format!("network {}", execution.network_access),
    ];
    if let Some(root) = execution.privilege.effective_root {
        parts.push(if root { "root" } else { "not root" }.to_owned());
    }
    if execution.privilege.non_interactive_sudo != SUDO_NOT_APPLICABLE {
        parts.push(format!("sudo {}", execution.privilege.non_interactive_sudo));
    }
    parts.join(ENVIRONMENT_SEPARATOR)
}

fn environment_facts(output: &EnvironmentPresentation) -> Vec<EnvironmentFact> {
    let runtime = format!(
        "{} {}{ENVIRONMENT_SEPARATOR}{}",
        output.runtime.name, output.runtime.version, output.scope
    );
    let container = if output.container.evidence.is_empty() {
        output.container.kind.to_owned()
    } else {
        format!(
            "{} ({})",
            output.container.kind,
            output.container.evidence.join(ENVIRONMENT_LIST_SEPARATOR)
        )
    };
    let mut facts = vec![
        fact(ENVIRONMENT_RUNTIME_LABEL, runtime),
        fact(ENVIRONMENT_CONTAINER_LABEL, container),
        fact(
            ENVIRONMENT_PACKAGES_LABEL,
            system_package_manager_fact(&output.os.system_package_manager),
        ),
        fact(
            ENVIRONMENT_WORKSPACE_LABEL,
            workspace_fact(&output.workspace),
        ),
    ];
    let groups = enabled_tool_groups(&output.tool_groups);
    if !groups.is_empty() {
        facts.push(fact(ENVIRONMENT_GROUPS_LABEL, groups.join(" ")));
    }
    facts.push(fact(
        ENVIRONMENT_INHERITANCE_LABEL,
        output.execution.environment_inheritance.to_owned(),
    ));
    facts
}

fn fact(label: &str, value: String) -> EnvironmentFact {
    EnvironmentFact {
        label: label.to_owned(),
        value,
    }
}

/// `apt 2.8.3 (apt-get)`. The executable is named only when it differs from the
/// manager, which is the case a caller would otherwise get wrong.
fn system_package_manager_fact(manager: &EnvironmentSystemPackageManager) -> String {
    if !manager.available {
        return ENVIRONMENT_NONE.to_owned();
    }
    let mut value = manager.name.to_owned();
    if let Some(version) = &manager.version {
        let _ = write!(value, " {version}");
    }
    if let Some(executable) = &manager.executable
        && executable != &manager.name
    {
        let _ = write!(value, " ({executable})");
    }
    value
}

fn workspace_fact(workspace: &EnvironmentWorkspace) -> String {
    let mut parts = vec![match workspace.git.repository.as_str() {
        GIT_REPOSITORY_YES => "git repository".to_owned(),
        GIT_REPOSITORY_NO => "no git repository".to_owned(),
        other => format!("git {other}"),
    }];
    match (
        &workspace.package_manager.declared,
        &workspace.package_manager.inferred,
    ) {
        (Some(declared), _) => parts.push(match &declared.version {
            Some(version) => format!("{} {version} declared", declared.name),
            None => format!("{} declared", declared.name),
        }),
        (None, Some(inferred)) => parts.push(format!("{inferred} inferred")),
        (None, None) => {}
    }
    parts.push(if workspace.package_manager.lockfiles.is_empty() {
        "no lockfiles".to_owned()
    } else {
        workspace
            .package_manager
            .lockfiles
            .join(ENVIRONMENT_LIST_SEPARATOR)
    });
    parts.join(ENVIRONMENT_SEPARATOR)
}

/// Only the groups that loaded. A disabled group is not a fact about the host.
fn enabled_tool_groups(groups: &EnvironmentGroups) -> Vec<&'static str> {
    [
        (groups.files, "files"),
        (groups.web, "web"),
        (groups.shell, "shell"),
        (groups.code, "code"),
        (groups.code_graph, "code-graph"),
    ]
    .into_iter()
    .filter_map(|(enabled, name)| enabled.then_some(name))
    .collect()
}

fn environment_error(error: ExecutionEnvironmentError) -> ToolError {
    let failure = match error {
        ExecutionEnvironmentError::Unavailable => ToolFailure::Other,
        ExecutionEnvironmentError::Cancelled => ToolFailure::Cancelled,
        ExecutionEnvironmentError::TimedOut => ToolFailure::Timeout,
    };
    ToolError::new(failure, error.to_string())
}

/// The live tail of a running command, rendered as a terminal would show it.
///
/// Progress chunks are byte-exact by contract, so a bar that redraws arrives as
/// a control stream rather than as lines. Splitting it on newlines would make an
/// hour of redrawing one row hundreds of kilobytes wide, and its frames would
/// evict everything printed before them from the retained window. Rendering on
/// the way in costs that window the width of one row instead.
///
/// One renderer per stream, because a row is a property of the stream that drew
/// it, and one buffer for both, because arrival order is what a reader saw.
#[derive(Default)]
struct ProgressTail {
    stdout: RowRenderer,
    stderr: RowRenderer,
    rows: String,
}

impl ProgressTail {
    fn push(&mut self, chunk: &ShellProgressChunk) -> String {
        let renderer = match chunk.stream {
            ShellStream::Stdout => &mut self.stdout,
            ShellStream::Stderr => &mut self.stderr,
        };
        renderer.push(&chunk.text, &mut self.rows);
        if self.rows.len() > PROGRESS_MAX_BYTES {
            let keep = PROGRESS_MAX_BYTES.saturating_sub(PROGRESS_TRUNCATED.len());
            let mut start = self.rows.len().saturating_sub(keep);
            while !self.rows.is_char_boundary(start) {
                start += 1;
            }
            self.rows = format!("{PROGRESS_TRUNCATED}{}", &self.rows[start..]);
        }
        // A bar writes no newline until it ends, so the row still being drawn is
        // the only thing there is to show for the length of the run.
        let mut text = self.rows.clone();
        self.stdout.row(&mut text);
        self.stderr.row(&mut text);
        text
    }
}

struct NativeProgressSink {
    id: Option<String>,
    event_tx: caudra_agent::EventSender,
    live_sink: Option<flume::Sender<ToolLive>>,
    body: Arc<caudra_agent::SharedBuf>,
    tail: Mutex<ProgressTail>,
}

/// Turns a code-graph phase into the one line the card shows while it runs.
///
/// A graph call is silent for seconds: it crawls, parses and ranks the whole
/// tree before it can answer anything. Without this the card is a bare spinner
/// for the entire time.
struct GraphPhaseSink {
    live_sink: flume::Sender<ToolLive>,
}

#[async_trait::async_trait]
impl GraphProgressSink for GraphPhaseSink {
    async fn publish(&self, progress: GraphProgress) {
        let label = match progress.files {
            0 => progress.phase.label().to_owned(),
            files => format!("{} {files} files", progress.phase.label()),
        };
        // Dropping a phase costs a frame of animation and nothing else, so a
        // full channel is never worth stalling the crawl for.
        let _ = self.live_sink.try_send(ToolLive::Annotation(label));
    }
}

impl WorkcellInvocation {
    fn graph_progress(&self, ctx: &ToolContext) -> Option<Box<dyn GraphProgressSink>> {
        ctx.live_sink
            .clone()
            .map(|live_sink| Box::new(GraphPhaseSink { live_sink }) as Box<dyn GraphProgressSink>)
    }
}

impl NativeProgressSink {
    fn new(ctx: &ToolContext) -> Self {
        Self {
            id: ctx.tool_use_id.clone(),
            event_tx: ctx.event_tx.clone(),
            live_sink: ctx.live_sink.clone(),
            body: Arc::new(caudra_agent::SharedBuf::new()),
            tail: Mutex::new(ProgressTail::default()),
        }
    }

    fn publish_live_buf(&self, ctx: &ToolContext) {
        if let Some(live) = &ctx.shell_live {
            live.attach(&self.body);
        }
        if let Some(sink) = &self.live_sink {
            let _ = sink.try_send(ToolLive::Buf(Arc::clone(&self.body)));
        }
    }
}

#[async_trait::async_trait]
impl ShellProgressSink for NativeProgressSink {
    async fn publish(&self, chunk: ShellProgressChunk) -> Result<(), String> {
        let text = self
            .tail
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(&chunk);
        self.body.set_lines(
            text.lines()
                .map(|line| SnapshotLine::plain(line.to_owned()))
                .collect(),
        );
        if let Some(id) = &self.id {
            self.event_tx.try_send(AgentEvent::ToolOutput {
                id: id.clone(),
                content: text,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::agent::mention_preamble;
    use caudra_agent::agent::tool_dispatch::{self, Emit};
    use caudra_agent::background::{BackgroundTasks, JobScope, ShellLive};
    use caudra_agent::cancel::CancelToken;
    use caudra_agent::permissions::pattern_recognition::{CommandObservation, ShellEffectStatus};
    use caudra_agent::permissions::{
        AutoNote, COMMAND_EXACT_PREFIX, COMMAND_TEMPLATE_PREFIX, ComposedRow, PermissionAnswer,
        PermissionCapabilityFamily, PermissionError, PermissionExecutorKind, PermissionLifetime,
        PermissionManager, PermissionMode, PermissionRequest, PermissionResourceAccess,
        PermissionResourceKind, PermissionResourceSelector, PermissionRowGrant, PermissionSubject,
        ScriptLanguage, permission_rule_covers_request, permission_rule_covers_resource,
        review::{COMMAND_TEMPLATE_EXECUTION_NOTICE, review_for_rule},
    };
    use caudra_agent::template::Vars;
    use caudra_agent::tools::execution::configure_tools;
    use caudra_agent::tools::{Deadline, FileReadTracker, STALE_READ_MSG, interpreter_ctx};
    use caudra_agent::{
        AgentMode, ContentBlock, Envelope, EventSender, History, Mention, Message, StoredSession,
        TaskCard, ToolFilter,
    };
    use caudra_config::{
        AgentConfig, DefaultEffect, Effect, ExecutionMode, PermissionRule, PermissionsConfig,
        ToolKey,
    };
    use caudra_storage::{
        StateDir,
        background::JobKind,
        id::SessionRef,
        permission_patterns::{
            ArgumentDomain, ArgumentRole, OptionLikePolicy, PatternToken, SlotCombinations,
        },
        permission_state::PermissionState,
        sessions::SessionDatabase,
        tool_outputs::ToolOutputStore,
    };
    use caudra_workspace::{OperationHandle, OperationId, OperationProgress, SequenceMetadata};
    use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
    use serde_json::json;
    use smol::lock::Mutex as AsyncMutex;
    use smol::net::TcpListener;
    use std::any::TypeId;
    use std::fs;
    use std::io::ErrorKind;
    use std::ops::RangeInclusive;
    use std::process::Command;
    use std::slice;
    use std::sync::Arc;
    use tempfile::TempDir;
    use test_case::test_case;
    use workcell::code::{
        CodeOutput, DEFAULT_TIMEOUT_MS as CODE_DEFAULT_TIMEOUT_MS,
        MAX_TIMEOUT_MS as CODE_MAX_TIMEOUT_MS, MAX_TIMEOUT_SECS as CODE_MAX_TIMEOUT_SECS,
    };
    use workcell::environment::{
        CommandDescriptor, ContainerDescriptor, DeclaredPackageManager,
        ExecutionEnvironmentExecution, ExecutionEnvironmentOutput, GitDescriptor, OsDescriptor,
        PackageManagerDescriptor, PrivilegeDescriptor, RuntimeDescriptor,
        SystemPackageManagerDescriptor, WorkspaceDescriptor,
    };
    use workcell::shell::bash::MAX_BASH_DEPTH;
    use workcell::shell::{
        DEFAULT_TIMEOUT_MS as SHELL_DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_SECS as SHELL_MAX_TIMEOUT_SECS,
    };

    const PATCH: &str = "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** End Patch";
    const PREVIEW_FLAG_MSG: &str = "a dry-run argument must fail the call rather than write";
    const EXPECT_MENTION_INLINED: &str = "a mention hands the model the file's contents";
    const EXPECT_SLICE_LEAVES_NO_RECORD: &str =
        "only a whole-file mention may clear a later edit's staleness check";
    const EXPECT_MENTION_NOTE: &str = "a mention that read nothing still tells the model why";
    const EXPECT_MENTION_HIDDEN: &str = "mention context never shows up in the transcript";
    const PATCH_STRUCTURED_MSG: &str = "a patch reports the files it changed, not a diff blob";
    const EXPECT_APPLIED_UNMARKED: &str = "a change that landed is reported by its diff alone";
    const EXPECT_NO_JSON_FOR_THE_MODEL: &str =
        "the model reads a rendering; serializing the record spends the payload twice";
    const EXPECT_SAME_AS_BATCHED: &str =
        "a call reads the same whether it was made directly or inside a batch";
    const FILTERABLE_MAKEFILE: &str = "all:\n\t@echo \"make[1]: Entering directory '/x'\"\n\t@echo \"real build line\"\n\t@echo \"make[1]: Leaving directory '/x'\"\n";
    const LOCKFILE: &str = "package-lock.json";
    const BROWSE_CONTENT_SENTINEL: &str = "content_not_authorized_by_a_names_only_grant";
    const PATTERN_PACKAGES: [&str; 3] = ["alpha", "beta", "gamma"];
    const PATTERN_COMMAND: &str = "cargo check -p alpha --tests";
    const INTERPRETER_SIBLING: &str = "cargo check -p core && python3 -c 'print(1)'";
    const INLINE_PYTHON: ShellOpacity = ShellOpacity::InlineScript {
        language: ScriptLanguage::Python,
    };
    const PATTERN_TIMEOUT_SECS: u64 = 1;
    const DEADLINE_COMMAND: &str = "cargo test";
    const DEADLINE_CODE: &str = "21 * 2";
    const GENERIC_NAME_REGEX: &str = "(?:alpha|beta|gamma|delta-[0-9]+)";
    const POSSIBLE_WORKDIRS_ATTRIBUTE: &str = "possible_workdirs";
    const POSSIBLE_WORKDIRS_LABEL: &str = "Possible working directories:";
    /// The descriptor pretty-prints to about 170 lines on a populated host; a
    /// rendering that ever approached that would have stopped being one.
    const ENVIRONMENT_MAX_MODEL_LINES: usize = 40;
    const REMOTE_ENVIRONMENT_MODEL_TEXT: &str = "authoritative remote environment text";
    /// Output that names every failure class, so a result is proven to be
    /// placed by Workcell's flags rather than by what the command printed.
    const MISLEADING_OUTPUT: &str = "cancelled: not found, permission denied, timeout exceeded";
    const RUN_DEADLINE: Duration = Duration::from_millis(50);
    const FAILING_EXIT_CODE: i32 = 1;
    const INVALID_REMOTE_RESULT: &str = "invalid remote Workcell result:";
    const EMBEDDED_SHELL_CALL: &str = "embedded-shell-call";
    const EMBEDDED_SHELL_READY: &[u8] = b"ready\n";
    const EMBEDDED_SHELL_OUTPUT: &str = "embedded-shell-terminal";
    const EMBEDDED_SHELL_RELEASE: &[u8] = b"release\n";
    const EMBEDDED_SHELL_TIMEOUT_SECS: u64 = 10;
    const EMBEDDED_SHELL_TEST_TIMEOUT: Duration = Duration::from_secs(20);
    const EMBEDDED_SHELL_TEST_EXPIRED: &str =
        "embedded shell smoke test exceeded its bounded deadline";
    const PREMATURE_SHELL_ACK: &str =
        "task event must be durably saved in parent history before acknowledgment";
    const SHELL_SUCCEEDED: &str = "succeeded";
    const SHELL_CANCELLED_STATE: &str = "cancelled";
    const OBSERVED_ROW: &str = "observed without a transcript";
    const LEADING_CD_COMMAND: &str = "cd ../workcell-mcp && grep -rn --include=*.rs -E";
    /// Names the child test a parent re-runs this binary for.
    const CHILD_TEST_ENV: &str = "CAUDRA_WORKCELL_CHILD_TEST";
    const HERDR_PANE_CHILD: &str = "tests::herdr_pane_environment_child";
    const HERDR_PANE_CHILD_PASSED: &str = "the herdr pane environment reached the command";
    const PANE_CANARY_SUFFIX: &str = "-canary";

    async fn bounded_shell_test<T>(operation: impl Future<Output = T>) -> T {
        future::race(operation, async {
            smol::Timer::after(EMBEDDED_SHELL_TEST_TIMEOUT).await;
            panic!("{EMBEDDED_SHELL_TEST_EXPIRED}");
        })
        .await
    }

    async fn settled_shell(scope: &JobScope, task_id: &str) -> TaskCard {
        loop {
            let revision = scope.revision();
            let card = scope.status(task_id).unwrap();
            if !card.active() {
                return card;
            }
            scope.wait_for_change(revision).await.unwrap();
        }
    }

    #[test_case(ExecutionMode::Auto, false; "timeout_routed_completion")]
    #[test_case(ExecutionMode::Async, false; "forced_async_completion")]
    #[test_case(ExecutionMode::Async, true; "cancel_cleans_parent_and_descendant")]
    fn embedded_shell_async_dispatch_smoke(mode: ExecutionMode, cancel: bool) {
        smol::block_on(bounded_shell_test(async {
            let root = TempDir::new().unwrap();
            let (_host, registry) = host_and_registry(root.path());
            let dir = StateDir::from_path(root.path().join("state"));
            let mut session =
                StoredSession::new(EMBEDDED_SHELL_CALL, root.path().to_str().unwrap());
            session.save(&dir).unwrap();
            let tasks = BackgroundTasks::spawn(dir.clone(), session.id)
                .await
                .unwrap();
            let scope = tasks.main_scope();
            let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
            let (events_tx, events) = flume::unbounded();
            ctx.event_tx = EventSender::new(events_tx, 0);
            let (_response_tx, response_rx) = flume::unbounded();
            ctx.user_response_rx = Some(Arc::new(AsyncMutex::new(response_rx)));
            ctx.jobs = Some(scope.clone());
            ctx.session_id = Some(SessionRef::from_id(session.id));
            ctx.tool_output_store = Some(Arc::new(ToolOutputStore::new(dir.clone())));
            ctx.config.shell_execution = mode.clone();
            if mode == ExecutionMode::Auto {
                ctx.config.shell_async_threshold_secs = 1;
            }
            ctx.config.shell_output_filter = false;
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let command = format!(
                "exec 3<>/dev/tcp/127.0.0.1/{port}; bash -c 'printf \"ready\\n\" >&3; IFS= read -r release <&3; printf \"{EMBEDDED_SHELL_OUTPUT}\\n\"' & wait"
            );
            let input = json!({"command":command, "timeoutSec":EMBEDDED_SHELL_TIMEOUT_SECS});
            let dispatch = tool_dispatch::run(
                &registry,
                None,
                EMBEDDED_SHELL_CALL.into(),
                SHELL_TOOL_NAME,
                &input,
                &ctx,
                Emit::Notify,
            );
            let (done, ()) = future::zip(dispatch, async {
                loop {
                    let event = events.recv_async().await.unwrap();
                    if let AgentEvent::PermissionRequest(request) = event.event {
                        assert!(
                            ctx.permissions
                                .answer(&request.id, PermissionAnswer::AllowOnce)
                        );
                        break;
                    }
                }
            })
            .await;
            assert!(!done.is_error, "{}", done.output.as_text());
            assert!(done.accounting.outcome.is_none());
            let receipt_text = done.output.as_text();
            let ToolOutput::Tasks(cards) = done.output else {
                panic!("expected shell admission receipt");
            };
            assert_eq!(cards.len(), 1);
            let card = &cards[0];
            assert_eq!(card.kind, JobKind::Shell);
            assert_eq!(card.shell.as_ref().unwrap().command, command);
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut ready = vec![0; EMBEDDED_SHELL_READY.len()];
            socket.read_exact(&mut ready).await.unwrap();
            assert_eq!(ready, EMBEDDED_SHELL_READY);
            assert!(scope.status(&card.task_id).unwrap().active());
            let mut eof = [0];
            assert!(future::poll_once(socket.read(&mut eof)).await.is_none());
            assert!(scope.claim_messages().unwrap().is_empty());
            if cancel {
                scope.cancel(&card.task_id).await.unwrap();
            } else {
                socket.write_all(EMBEDDED_SHELL_RELEASE).await.unwrap();
            }
            let terminal = settled_shell(&scope, &card.task_id).await;
            assert_eq!(
                terminal.state,
                if cancel {
                    SHELL_CANCELLED_STATE
                } else {
                    SHELL_SUCCEEDED
                }
            );
            assert_eq!(socket.read(&mut eof).await.unwrap(), 0);
            assert!(scope.claim_messages().unwrap().is_empty());
            let record = SessionDatabase::open(&dir)
                .unwrap()
                .background_tasks(session.id)
                .unwrap()
                .pop()
                .unwrap();
            assert!(!record.receipt_accepted);
            assert_eq!(
                record.events.iter().filter(|event| event.terminal).count(),
                1
            );
            assert!(record.output_ref.is_some());
            if !cancel {
                let output: ToolOutput =
                    serde_json::from_value(record.outcome.unwrap()["shell"].clone()).unwrap();
                let ToolOutput::Shell(output) = output else {
                    panic!("expected terminal shell output");
                };
                assert_eq!(output.exit_code, Some(0));
                assert!(!output.timed_out);
                assert_eq!(output.stdout.matches(EMBEDDED_SHELL_OUTPUT).count(), 1);
                assert!(output.stderr.is_empty());
            }
            let receipt = Message {
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: EMBEDDED_SHELL_CALL.into(),
                    content: receipt_text,
                    is_error: false,
                    output_ref: None,
                }],
                ..Message::default()
            };
            session.replace_messages(History::new(vec![receipt.clone()]).into_items());
            session.save(&dir).unwrap();
            scope
                .settle_launches(slice::from_ref(&receipt))
                .await
                .unwrap();
            let messages = scope.claim_messages().unwrap();
            let delivered = messages
                .iter()
                .filter_map(|message| message.task_event.as_ref())
                .collect::<Vec<_>>();
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].task_id, card.task_id);
            assert_eq!(
                scope.accept_messages(&messages).await.unwrap_err(),
                PREMATURE_SHELL_ACK
            );
            let mut history = vec![receipt];
            history.extend(messages.clone());
            session.replace_messages(History::new(history).into_items());
            session.save(&dir).unwrap();
            scope.accept_messages(&messages).await.unwrap();
            assert!(scope.claim_messages().unwrap().is_empty());
            assert!(!scope.pending());
            assert!(
                !events
                    .try_iter()
                    .any(|event| matches!(event.event, AgentEvent::ToolDone(_)))
            );
            tasks.shutdown().await.unwrap();
        }));
    }

    #[test_case(ExecutionMode::Sync; "sync")]
    #[test_case(ExecutionMode::Auto; "auto")]
    #[test_case(ExecutionMode::Async; "async_mode")]
    fn embedded_shell_catalog_execution_contract(mode: ExecutionMode) {
        let root = TempDir::new().unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let original_schema = registry.get(SHELL_TOOL_NAME).unwrap().tool.schema();
        for supported in [false, true] {
            let config = AgentConfig {
                shell_execution: mode.clone(),
                ..AgentConfig::default()
            };
            let mut definitions = registry.definitions_split(
                &Vars::new(),
                &DescriptionContext {
                    filter: &ToolFilter::Only(vec![SHELL_TOOL_NAME.into()]),
                    audience: ToolAudience::MAIN,
                    workflows_available: false,
                },
                false,
                &[SHELL_TOOL_NAME],
            );
            configure_tools(
                &mut definitions.declared,
                &mut definitions.deferred,
                &config,
                false,
                supported,
            );
            let Some(effective) = mode.effective(supported) else {
                assert!(definitions.deferred.is_empty());
                continue;
            };
            assert_eq!(definitions.deferred.len(), 1);
            let definition = &definitions.deferred[0].definition;
            assert_eq!(definition["input_schema"], original_schema);
            assert!(
                definition["input_schema"]["properties"]
                    .get("background")
                    .is_none()
            );
            let description = definition["description"].as_str().unwrap().to_lowercase();
            assert!(description.contains("timeout"));
            match effective {
                ExecutionMode::Sync => {
                    for forbidden in ["background", "async", "receipt"] {
                        assert!(!description.contains(forbidden), "{description}");
                    }
                }
                ExecutionMode::Auto => {
                    assert!(description.contains("120 seconds"));
                    assert!(description.contains("receipt"));
                }
                ExecutionMode::Async => {
                    for forbidden in ["foreground", "synchronous", "holds the call", "120 seconds"]
                    {
                        assert!(!description.contains(forbidden), "{description}");
                    }
                    assert!(description.contains("receipt"));
                }
            }
        }
    }

    #[test]
    fn selected_remote_endpoint_cannot_also_be_generic_mcp() {
        let selected = url::Url::parse("https://workcell.example/mcp").unwrap();
        assert!(generic_mcp_endpoint_collision(
            &selected,
            ["https://other.example/mcp", selected.as_str()]
        ));
        assert!(!generic_mcp_endpoint_collision(
            &selected,
            ["https://workcell.example/other"]
        ));
    }

    #[test]
    fn remote_catalog_requires_canonical_order_contracts_schemas_and_effects() {
        let specs = canonical_remote_specs();
        let names: Vec<_> = specs.iter().map(|spec| spec.name).collect();
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
                "websearch",
                "webfetch",
                "shell",
                "code_map",
                "code_context",
                "code_refs",
                "code_impact",
                "code_expand",
                "python_execution",
                "execution_environment",
            ]
        );
        let catalog: Vec<_> = specs.iter().map(OwnedToolSpec::from).collect();
        assert!(canonical_remote_catalog(&catalog));
        for spec in &catalog {
            let expected = match spec.name.as_str() {
                "python_execution" => ToolEffect::Isolated,
                "file_write"
                | "file_edit"
                | "file_apply_patch"
                | "shell"
                | "execution_environment" => ToolEffect::Mutating,
                _ => ToolEffect::ReadOnly,
            };
            assert_eq!(ToolKind::from_name(&spec.name).unwrap().effect(), expected);
        }
        let mut reordered = catalog.clone();
        reordered.reverse();
        assert!(canonical_remote_catalog(&reordered));
        let mut changed = catalog;
        changed[0].contract_id = "different.contract".into();
        assert!(!canonical_remote_catalog(&changed));
    }

    /// A remote command runs on the server's clock, so the client has to outwait
    /// the deadline it asked for. Giving up first cancels the operation, which is
    /// why the shell ceiling is not the one every other tool uses.
    #[test_case(ToolKind::Shell => SHELL_EXECUTION_TIMEOUT ; "a command waits out the deadline it was given")]
    #[test_case(ToolKind::FileRead => REMOTE_EXECUTION_TIMEOUT ; "a read keeps the ordinary ceiling")]
    #[test_case(ToolKind::Code => REMOTE_EXECUTION_TIMEOUT ; "so does the code worker, which bounds itself")]
    fn the_execution_ceiling_is_raised_only_for_the_shell(kind: ToolKind) -> Duration {
        remote_execution_ceiling(kind)
    }

    #[test_case(None; "default")]
    #[test_case(Some(1); "minimum")]
    #[test_case(Some(SHELL_MAX_TIMEOUT_SECS); "maximum")]
    #[test_case(Some(0); "zero")]
    #[test_case(Some(SHELL_MAX_TIMEOUT_SECS + 1); "above_maximum")]
    fn shell_timeout_metadata_matches_the_prepared_execution(timeout_sec: Option<u64>) {
        let root = TempDir::new().unwrap();
        let (host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), registry, CancelToken::none());
        let input = json!({"command": DEADLINE_COMMAND, "timeoutSec": timeout_sec});
        let invocation = WorkcellInvocation {
            host: Arc::clone(&host.inner),
            input: Input::parse(ToolKind::Shell, input.clone()).unwrap(),
            raw_input: Some(input.clone()),
            prepared: Mutex::new(None),
        };
        let timeout = invocation.shell_timeout();
        let failure = smol::block_on(invocation.preflight(&ctx))
            .err()
            .map(|error| error.failure);
        assert_eq!(
            failure,
            timeout.is_none().then_some(ToolFailure::InvalidInput)
        );
        assert_eq!(invocation.shell_timeout(), timeout);
        assert_eq!(invocation.permission_input(), Some(&input));
        if let Some(timeout) = timeout {
            let prepared = smol::block_on(invocation.take_prepared(&ctx)).unwrap();
            let PreparedExecution::Shell(_, shell) = prepared.execution else {
                panic!("expected prepared shell");
            };
            assert_eq!(timeout, Duration::from_millis(shell.timeout_ms()));
        }
    }

    /// What a header may promise is exactly what the executor will enforce, so
    /// the input is put to the executor's own type rather than read by a copy of
    /// its rule. Anything that type refuses runs under no deadline at all.
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND }) => Some(Duration::from_millis(SHELL_DEFAULT_TIMEOUT_MS)) ; "an omitted shell deadline is the default one")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeoutSec": Value::Null }) => Some(Duration::from_millis(SHELL_DEFAULT_TIMEOUT_MS)) ; "an explicit null arrives as omitted")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeoutSec": 0 }) => None ; "zero is refused rather than read as a limit")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeoutSec": 1 }) => Some(Duration::from_secs(1)) ; "the smallest shell deadline is one second")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeoutSec": SHELL_MAX_TIMEOUT_SECS }) => Some(Duration::from_millis(SHELL_MAX_TIMEOUT_MS)) ; "the shell maximum is itself allowed")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeoutSec": SHELL_MAX_TIMEOUT_SECS + 1 }) => None ; "a shell deadline past the maximum is refused rather than clamped")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeout": SHELL_MAX_TIMEOUT_SECS }) => None ; "a count under the millisecond key is refused")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeoutSec": -1 }) => None ; "a negative deadline names nothing")]
    #[test_case(SHELL_TOOL_NAME, json!({ "command": DEADLINE_COMMAND, "timeoutSec": "30m" }) => None ; "a deadline that is not a number names nothing")]
    #[test_case(SHELL_TOOL_NAME, json!({ "timeoutSec": 1 }) => None ; "a call with no command never runs")]
    #[test_case(PYTHON_EXECUTION_TOOL_NAME, json!({ "code": DEADLINE_CODE }) => Some(Duration::from_millis(CODE_DEFAULT_TIMEOUT_MS)) ; "an omitted code deadline is its own default")]
    #[test_case(PYTHON_EXECUTION_TOOL_NAME, json!({ "code": DEADLINE_CODE, "timeoutSec": 0 }) => None ; "zero is refused by the code worker")]
    #[test_case(PYTHON_EXECUTION_TOOL_NAME, json!({ "code": DEADLINE_CODE, "timeoutSec": CODE_MAX_TIMEOUT_SECS }) => Some(Duration::from_millis(CODE_MAX_TIMEOUT_MS)) ; "the code maximum is allowed")]
    #[test_case(PYTHON_EXECUTION_TOOL_NAME, json!({ "code": DEADLINE_CODE, "timeoutSec": CODE_MAX_TIMEOUT_SECS + 1 }) => None ; "a code deadline past the maximum is refused")]
    #[test_case(PYTHON_EXECUTION_TOOL_NAME, json!({ "code": DEADLINE_CODE, "timeout": CODE_MAX_TIMEOUT_SECS }) => None ; "the code worker refuses the millisecond key too")]
    #[test_case("file_read", json!({ "timeoutSec": 1 }) => None ; "a tool without a deadline has none to report")]
    fn a_header_deadline_is_the_one_the_executor_will_enforce(
        tool: &str,
        raw_input: Value,
    ) -> Option<Duration> {
        effective_timeout(tool, &raw_input)
    }

    const PROJECT_CWD: &str = "/project";

    /// A running card has only the arguments, a settled one has the result,
    /// and the header must not change words between the two.
    #[test_case(SHELL_TOOL_NAME, json!({}) => Some(CURRENT_WORKDIR.to_owned()) ; "an omitted workdir is the project")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": Value::Null }) => Some(CURRENT_WORKDIR.to_owned()) ; "an explicit null arrives as omitted")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "" }) => Some(CURRENT_WORKDIR.to_owned()) ; "an empty workdir is the project")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "./" }) => Some(CURRENT_WORKDIR.to_owned()) ; "a dot is the project")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "/project/" }) => Some(CURRENT_WORKDIR.to_owned()) ; "the project spelled absolute is still the project")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "crates/core/" }) => Some("crates/core".to_owned()) ; "a subdirectory stays relative")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "/project/a/../docs" }) => Some("docs".to_owned()) ; "an absolute path inside folds to relative")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "../sibling" }) => Some("/sibling".to_owned()) ; "leaving the project reads absolute")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "/elsewhere" }) => Some("/elsewhere".to_owned()) ; "an outside path stays absolute")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": "/project2" }) => Some("/project2".to_owned()) ; "a shared text prefix is not containment")]
    #[test_case(SHELL_TOOL_NAME, json!({ "workdir": 1 }) => None ; "a workdir that is not a path names nothing")]
    #[test_case(PYTHON_EXECUTION_TOOL_NAME, json!({ "workdir": "crates" }) => None ; "a tool without a workdir has none to report")]
    fn a_header_workdir_is_spelled_the_way_the_result_will_spell_it(
        tool: &str,
        raw_input: Value,
    ) -> Option<String> {
        requested_workdir(tool, &raw_input, Path::new(PROJECT_CWD))
    }

    #[test_case(true; "cancelled_before_first_execution_poll")]
    #[test_case(false; "expired_before_first_execution_poll")]
    fn unpolled_remote_execution_remains_releasable(cancelled: bool) {
        let temp = TempDir::new().unwrap();
        let (trigger, cancel) = CancelToken::new();
        if cancelled {
            trigger.cancel();
        }
        let ctx = context(temp.path(), Arc::new(ToolRegistry::new()), cancel);
        let deadline = if cancelled {
            Instant::now() + REMOTE_EXECUTION_TIMEOUT
        } else {
            Instant::now()
        };
        let mut started = false;
        let mut polled = false;
        let result = smol::block_on(race_remote_execution(&ctx, deadline, &mut started, async {
            polled = true;
        }));
        assert!(!started);
        assert!(!polled);
        if cancelled {
            const CANCELLED: &str = "cancelled";
            assert_eq!(result.unwrap_err(), CANCELLED);
        } else {
            assert_eq!(result.unwrap(), None);
        }
    }

    #[test]
    fn polled_remote_execution_remains_uncertain_on_cancellation() {
        const CANCELLED: &str = "cancelled";
        let temp = TempDir::new().unwrap();
        let (trigger, cancel) = CancelToken::new();
        let ctx = context(temp.path(), Arc::new(ToolRegistry::new()), cancel);
        let mut started = false;
        let result = smol::block_on(async {
            let mut execution = Box::pin(race_remote_execution(
                &ctx,
                Instant::now() + REMOTE_EXECUTION_TIMEOUT,
                &mut started,
                future::pending::<()>(),
            ));
            assert!(future::poll_once(&mut execution).await.is_none());
            trigger.cancel();
            execution.await
        });
        assert_eq!(result.unwrap_err(), CANCELLED);
        assert!(started);
    }

    #[test]
    fn remote_file_result_uses_the_embedded_specialized_adapter() {
        let output = FileReadOutput::File {
            path: "remote/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            text: "fn main() {}".into(),
            numbered_text: "1: fn main() {}".into(),
            line_start: 1,
            line_end: 1,
            total_lines: 1,
            truncated: false,
        };
        let model_output = output.model_text().into_owned();
        let embedded = file_read_result(output.clone());
        let remote = remote_result(
            ToolKind::FileRead,
            &Input::FileRead(FileReadInput {
                file_path: "remote/src/lib.rs".into(),
                offset: None,
                limit: None,
            }),
            RemoteToolResultEnvelope {
                model_output: model_output.clone(),
                structured_content: serde_json::to_value(output).expect("structured result"),
                is_error: false,
            },
        );
        assert_eq!(
            remote.output.unwrap().as_text(),
            embedded.output.unwrap().as_text()
        );
        assert_eq!(remote.model_output.as_deref(), Some(model_output.as_str()));
        assert!(remote.remote_written_paths);
    }

    fn environment_fixture(populated: bool) -> ExecutionEnvironmentOutput {
        ExecutionEnvironmentOutput {
            version: "1",
            snapshot_revision: "remote-snapshot".into(),
            scope: "remote host",
            os: OsDescriptor {
                family: "remote-os",
                architecture: "remote-arch",
                path_style: "posix",
                kernel_release: populated.then(|| "9.8.7".into()),
                distribution: populated.then(|| "Remote Distribution".into()),
                wsl: populated,
                system_package_manager: SystemPackageManagerDescriptor {
                    name: "apt",
                    available: populated,
                    executable: populated.then_some("apt-get"),
                    version: populated.then(|| "2.8.3".into()),
                },
            },
            runtime: RuntimeDescriptor {
                name: "remote-runtime",
                version: "9.8.7",
            },
            execution: ExecutionEnvironmentExecution {
                shell: "remote-shell",
                sandbox: "remote-sandbox",
                network_access: "remote-network",
                environment_inheritance: "remote-inheritance",
                privilege: PrivilegeDescriptor {
                    effective_root: populated.then_some(false),
                    non_interactive_sudo: if populated {
                        "available"
                    } else {
                        SUDO_NOT_APPLICABLE
                    },
                },
            },
            container: ContainerDescriptor {
                kind: "remote-container",
                evidence: if populated {
                    vec!["cgroup", "marker"]
                } else {
                    vec![]
                },
            },
            workspace: WorkspaceDescriptor {
                git: GitDescriptor {
                    available: populated,
                    repository: if populated {
                        GIT_REPOSITORY_YES
                    } else {
                        GIT_REPOSITORY_NO
                    },
                },
                package_manager: PackageManagerDescriptor {
                    declared: populated.then(|| DeclaredPackageManager {
                        name: "pnpm".into(),
                        version: Some("10.0.0".into()),
                    }),
                    inferred: Some("npm".into()),
                    lockfiles: if populated {
                        vec!["pnpm-lock.yaml"]
                    } else {
                        vec![]
                    },
                },
            },
            tool_groups: ToolGroupDisclosure {
                files: populated,
                web: populated,
                shell: populated,
                code: populated,
                code_graph: populated,
            },
            commands: vec![
                CommandDescriptor {
                    id: "remote-command",
                    available: true,
                    version: Some("3.2.1".into()),
                },
                CommandDescriptor {
                    id: "unversioned-command",
                    available: true,
                    version: None,
                },
                CommandDescriptor {
                    id: "missing-command",
                    available: false,
                    version: None,
                },
            ],
        }
    }

    #[test_case(true, false; "populated")]
    #[test_case(false, false; "sparse")]
    #[test_case(true, true; "remote_error_flag")]
    fn remote_environment_card_matches_local_and_restores(populated: bool, is_error: bool) {
        let output = environment_fixture(populated);
        let structured_content = serde_json::to_value(&output).expect("serialized Workcell output");
        let local = environment_result(ExecutionEnvironmentResult {
            output,
            model_text: REMOTE_ENVIRONMENT_MODEL_TEXT.into(),
        });
        let remote = remote_result(
            ToolKind::Environment,
            &Input::Environment,
            RemoteToolResultEnvelope {
                structured_content,
                model_output: REMOTE_ENVIRONMENT_MODEL_TEXT.into(),
                is_error,
            },
        );
        assert_eq!(remote.is_error, is_error);
        assert_eq!(
            remote.model_output.as_deref(),
            Some(REMOTE_ENVIRONMENT_MODEL_TEXT)
        );
        assert!(local.model_output.is_none());
        let local = local.output.expect("local card");
        let remote = remote.output.expect("remote card");
        assert!(matches!(remote, ToolOutput::Environment { .. }));
        assert_eq!(
            serde_json::to_value(&remote).unwrap(),
            serde_json::to_value(&local).unwrap()
        );
        assert_eq!(remote.as_text(), local.as_text());
        let restored: ToolOutput =
            serde_json::from_str(&serde_json::to_string(&remote).unwrap()).unwrap();
        assert!(matches!(restored, ToolOutput::Environment { .. }));
        assert_eq!(restored.as_text(), remote.as_text());
        let text = restored.as_text();
        for fact in [
            "remote-os/remote-arch",
            "remote-shell",
            "remote-runtime",
            "remote-container",
            "remote-inheritance",
            "remote-command",
        ] {
            assert!(text.contains(fact), "{text}");
        }
        if populated {
            assert!(text.contains("apt 2.8.3 (apt-get)"), "{text}");
            assert!(text.contains("pnpm 10.0.0 declared"), "{text}");
        } else {
            assert!(text.contains("npm inferred"), "{text}");
        }
    }

    #[test_case(json!(null), false; "null")]
    #[test_case(json!({}), false; "missing_fields")]
    #[test_case(json!({"os": false}), false; "wrong_type")]
    #[test_case(json!({"error": "unavailable"}), true; "remote_error_payload")]
    fn malformed_remote_environment_is_an_error(structured_content: Value, is_error: bool) {
        let result = remote_result(
            ToolKind::Environment,
            &Input::Environment,
            RemoteToolResultEnvelope {
                structured_content,
                model_output: REMOTE_ENVIRONMENT_MODEL_TEXT.into(),
                is_error,
            },
        );
        assert!(result.is_error);
        assert!(
            result
                .output
                .unwrap_err()
                .starts_with(INVALID_REMOTE_RESULT)
        );
    }

    #[test_case(true; "retention_gap")]
    #[test_case(false; "intra_page_gap")]
    fn remote_progress_reports_a_gap_before_ordered_output(retention_gap: bool) {
        let root = TempDir::new().unwrap();
        let registry = Arc::new(ToolRegistry::new());
        let (_, cancel) = CancelToken::new();
        let mut ctx = context(root.path(), registry, cancel);
        let (sender, receiver) = flume::unbounded();
        ctx.live_sink = Some(sender);
        let execution_id = OperationId::new("execution").unwrap();
        let mut status = OperationStatus {
            handle: OperationHandle {
                preparation_id: OperationId::new("preparation").unwrap(),
                invocation_id: Some(OperationId::new("invocation").unwrap()),
                execution_id: Some(execution_id.clone()),
                expires_at_unix_ms: Some(1),
            },
            state: OperationState::Running,
            progress: vec![
                OperationProgress {
                    execution_id: execution_id.clone(),
                    sequence: 4,
                    kind: OperationProgressKind::Started,
                    chunk: String::new(),
                },
                OperationProgress {
                    execution_id,
                    sequence: if retention_gap { 5 } else { 6 },
                    kind: OperationProgressKind::Stdout,
                    chunk: "ordered output".into(),
                },
            ],
            progress_metadata: SequenceMetadata {
                first_retained_sequence: Some(4),
                next_sequence: if retention_gap { 6 } else { 7 },
                gap_before_first: retention_gap,
            },
        };
        let mut progress = RemoteProgress::new(&ctx);
        assert_eq!(progress.publish(&status), retention_gap);
        assert!(matches!(
            receiver.recv().unwrap(),
            ToolLive::Annotation(message) if message == REMOTE_PROGRESS_GAP
        ));
        if retention_gap {
            assert!(matches!(receiver.recv().unwrap(), ToolLive::Buf(_)));
            assert!(progress.publish(&status));
            assert_eq!(progress.after_sequence(), 5);
            assert!(receiver.try_recv().is_err());
            return;
        }
        assert!(!progress.publish(&status));
        assert!(receiver.try_recv().is_err());
        assert_eq!(progress.after_sequence(), 0);
        let mut without_sink = RemoteProgress::new(&ctx);
        without_sink.sink = None;
        assert!(!without_sink.publish(&status));
        assert_eq!(without_sink.next_sequence, progress.next_sequence);
        status.progress[0].sequence = 1;
        status.progress[1].sequence = 3;
        status.progress_metadata.first_retained_sequence = Some(1);
        status.progress_metadata.next_sequence = 4;
        assert!(!progress.publish(&status));
        assert_eq!(progress.after_sequence(), 1);
        status.progress[0].sequence = 2;
        assert!(progress.publish(&status));
        assert_eq!(progress.after_sequence(), 3);
        let published = receiver.len();
        assert!(progress.publish(&status));
        assert_eq!(receiver.len(), published);
        status.progress.clear();
        assert!(progress.publish(&status));
        status.progress_metadata.next_sequence = 5;
        assert!(!progress.publish(&status));
        assert_eq!(progress.after_sequence(), 3);
    }

    pub(super) fn context(
        root: &Path,
        registry: Arc<ToolRegistry>,
        cancel: CancelToken,
    ) -> ToolContext {
        context_with_mode(
            root,
            registry,
            cancel,
            AgentMode::Build,
            DefaultEffect::Allow,
        )
    }

    fn context_with_mode(
        root: &Path,
        registry: Arc<ToolRegistry>,
        cancel: CancelToken,
        mode: AgentMode,
        default_effect: DefaultEffect,
    ) -> ToolContext {
        let (tx, _rx) = flume::unbounded::<Envelope>();
        let event_tx = EventSender::new(tx, 0);
        let permissions = PermissionManager::new_nonpersistent(
            PermissionsConfig {
                default: default_effect,
                rules: (default_effect == DefaultEffect::Deny)
                    .then(|| PermissionRule {
                        tool: ToolKey::native("file_apply_patch"),
                        scope: Some("*".into()),
                        effect: Effect::Deny,
                    })
                    .into_iter()
                    .collect(),
                ..PermissionsConfig::default()
            },
            root.to_path_buf(),
            Arc::default(),
        );
        let mut ctx = interpreter_ctx(
            &mode,
            &event_tx,
            cancel,
            Arc::new(permissions),
            Arc::new(FileReadTracker::new()),
            None,
            registry,
        );
        ctx.config.stale_read_check = false;
        // Most shell cases below use `cat` or `rg` as a stand-in for some
        // read-only command, and are about confinement, plan mode, or `cd`
        // branches rather than about which command was chosen. The redirect has
        // its own cases, which turn it back on.
        ctx.config.shell_native_redirect = ShellNativeRedirect::Off;
        ctx.config.shell_workdir_redirect = false;
        ctx
    }

    fn host_and_registry(root: &Path) -> (WorkcellHost, Arc<ToolRegistry>) {
        let host = WorkcellHost::new(root, None).expect("Workcell host");
        let registry = Arc::new(ToolRegistry::new());
        host.register(&registry).expect("Workcell registration");
        (host, registry)
    }

    /// Websearch's prose includes the current year. Removing only descriptions
    /// keeps this deterministic while freezing every validation keyword.
    fn schema_contract(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "description")
                    .map(|(key, value)| (key.clone(), schema_contract(value)))
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.iter().map(schema_contract).collect()),
            value => value.clone(),
        }
    }

    fn canonical_input_schemas() -> Vec<(&'static str, Value)> {
        const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";
        const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

        vec![
            (
                "file_read",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "properties": {
                        "filePath": { "type": "string", "minLength": 1 },
                        "offset": { "type": "integer", "minimum": 1, "maximum": MAX_SAFE_INTEGER },
                        "limit": { "type": "integer", "minimum": 0, "maximum": MAX_SAFE_INTEGER }
                    },
                    "required": ["filePath"]
                }),
            ),
            (
                "file_glob",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "minLength": 1 },
                        "path": { "type": "string", "minLength": 1 }
                    },
                    "required": ["pattern"]
                }),
            ),
            (
                "file_grep",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "minLength": 1 },
                        "path": { "type": "string", "minLength": 1 },
                        "include": { "type": "string", "minLength": 1 },
                        "-A": { "type": "integer", "minimum": 0 },
                        "-B": { "type": "integer", "minimum": 0 },
                        "-C": { "type": "integer", "minimum": 0 },
                        "head_limit": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["pattern"]
                }),
            ),
            (
                "file_write",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "filePath": { "type": "string", "minLength": 1 },
                        "content": { "type": "string" }
                    },
                    "required": ["filePath", "content"]
                }),
            ),
            (
                "file_edit",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "filePath": { "type": "string", "minLength": 1 },
                        "oldString": { "type": "string", "minLength": 1 },
                        "newString": { "type": "string" },
                        "replaceAll": { "type": "boolean" }
                    },
                    "required": ["filePath", "oldString", "newString"]
                }),
            ),
            (
                "file_apply_patch",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "patchText": { "type": "string", "minLength": 1 } },
                    "required": ["patchText"]
                }),
            ),
            (
                "file_index",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "path": { "type": "string", "minLength": 1 } },
                    "required": ["path"]
                }),
            ),
            (
                "websearch",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": { "type": "string", "maxLength": 512 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 25 },
                        "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 60 }
                    },
                    "required": ["query"]
                }),
            ),
            (
                "webfetch",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "url": { "type": "string" },
                        "format": { "type": "string", "enum": ["markdown", "text", "html"] },
                        "pdfMode": { "type": "string", "enum": ["extract", "attachment"] },
                        "timeout": { "type": "integer", "minimum": 1, "maximum": 60 }
                    },
                    "required": ["url"]
                }),
            ),
            (
                "shell",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": { "type": "string", "minLength": 1 },
                        "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 21_600, "default": 120 },
                        "workdir": { "type": "string", "minLength": 1 }
                    },
                    "required": ["command"]
                }),
            ),
            (
                "python_execution",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "code": { "type": "string", "minLength": 1, "maxLength": 65_536 },
                        "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 30, "default": 5 }
                    },
                    "required": ["code"]
                }),
            ),
            (
                "code_map",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1 }
                    }
                }),
            ),
            (
                "code_context",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "task": { "type": "string", "minLength": 1 },
                        "path": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["task"]
                }),
            ),
            (
                "code_refs",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "symbol": { "type": "string", "minLength": 1 },
                        "direction": { "type": "string", "enum": ["callers", "callees"], "default": "callers" },
                        "path": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["symbol"]
                }),
            ),
            (
                "code_impact",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "symbol": { "type": "string", "minLength": 1 },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 8, "default": 3 },
                        "path": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["symbol"]
                }),
            ),
            (
                "code_expand",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "symbol": { "type": "string", "minLength": 1 },
                        "path": { "type": "string" }
                    },
                    "required": ["symbol"]
                }),
            ),
            (
                "execution_environment",
                json!({
                    "$schema": DRAFT_07,
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                }),
            ),
        ]
    }

    fn assert_embedded_file_read(host: &WorkcellHost) {
        const NOT_EMBEDDED: &str = "the default constructor must register the in-process Workcell";

        let registry = ToolRegistry::new();
        host.register(&registry).expect("Workcell registration");
        let registered = registry.get("file_read").expect("registered file_read");
        assert_eq!(
            registered.tool.as_ref().type_id(),
            TypeId::of::<WorkcellTool>(),
            "{NOT_EMBEDDED}"
        );
        assert!(
            matches!(registered.source, ToolSource::Native { ref owner, trusted: true, .. } if owner.as_ref() == OWNER),
            "{NOT_EMBEDDED}"
        );
    }

    #[test]
    fn default_public_constructors_start_embedded_workcell() {
        let root = TempDir::new().expect("tempdir");
        let development = WorkcellHost::new(root.path(), None).expect("development Workcell host");
        let production =
            WorkcellHost::new_production(root.path(), None).expect("production Workcell host");

        assert_embedded_file_read(&development);
        assert_embedded_file_read(&production);
    }

    /// A future mtime stands in for another process touching the file, without
    /// racing the filesystem's timestamp resolution.
    fn bump_mtime(path: &Path) {
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("opening the file to move its mtime")
            .set_modified(future)
            .expect("moving the mtime forward");
    }

    /// The shared context disables the stale check, so every test that means to
    /// exercise it has to opt back in and record the read it is invalidating.
    fn tracking_context(
        root: &Path,
        registry: Arc<ToolRegistry>,
        read: &Path,
        stale: bool,
    ) -> ToolContext {
        let mut ctx = context(root, registry, CancelToken::none());
        ctx.config.stale_read_check = true;
        ctx.file_tracker.record_read(read);
        if stale {
            bump_mtime(read);
        }
        ctx
    }

    #[test_case("git status > /tmp/status", true; "output_redirect")]
    #[test_case("> /tmp/status git status", true; "leading_redirect")]
    #[test_case("cat 2>>errors", true; "fd_redirect")]
    #[test_case("cat >| clobber", true; "clobbering_redirect")]
    #[test_case("cat &> combined", true; "combined_redirect")]
    #[test_case("cat >& combined", true; "descriptor_syntax_file_target")]
    #[test_case("cat >&2log", true; "descriptor_prefixed_file_target")]
    #[test_case("cat <input", true; "input_redirect")]
    #[test_case("cat >/dev/nullx", true; "a_path_beginning_with_the_device")]
    #[test_case("cat >/dev/nul", true; "a_path_shorter_than_the_device")]
    #[test_case("cat >/dev/null/../../etc/passwd", true; "a_path_leading_through_the_device")]
    #[test_case("cat >& /dev/null", true; "the_device_behind_a_descriptor_ampersand")]
    #[test_case("cat <<EOF\nvalue\nEOF", true; "heredoc")]
    #[test_case("cat <<<value", true; "here_string")]
    #[test_case("git status # '\n> victim", true; "quote_in_comment_before_redirect")]
    #[test_case("printf foo#bar > output", true; "hash_inside_word_before_redirect")]
    #[test_case(r"printf $'a\'b' > output", true; "redirect_after_ansi_c_quote")]
    #[test_case("cargo check 2>&1 | head -40", false; "stderr_to_stdout")]
    #[test_case("git log --oneline 2>/dev/null | head -60", false; "stderr_discarded")]
    #[test_case("cargo check >/dev/null 2>&1", false; "discarded_then_duplicated")]
    #[test_case("cat &>/dev/null", false; "both_streams_discarded")]
    #[test_case("cat 2>> /dev/null", false; "appended_to_the_device")]
    #[test_case("cat < /dev/null", false; "read_from_the_device")]
    #[test_case("cargo check >&2", false; "stdout_to_stderr")]
    #[test_case("cargo check 1>&2", false; "explicit_stdout_to_stderr")]
    #[test_case("exec 3<&0", true; "input_descriptor_state_change")]
    #[test_case("cargo check >& 2", false; "spaced_descriptor_duplicate")]
    #[test_case("cargo check 2>&-", false; "descriptor_close")]
    #[test_case("echo ok # > ignored", false; "redirect_inside_comment")]
    #[test_case("echo '%s > %s' left right", false; "single_quoted_literal")]
    #[test_case(r#"echo ">""#, false; "double_quoted_literal")]
    #[test_case(r"echo \>", false; "escaped_literal")]
    #[test_case(r"echo $'a\'b'", true; "undecoded_ansi_c_quoted_literal")]
    #[test_case(r"echo $'>'", true; "undecoded_ansi_c_quoted_redirect_literal")]
    fn shell_commands_hiding_operands_require_exact_authority(command: &str, expected: bool) {
        let root = TempDir::new().expect("tempdir");
        let intent = shell_preflight_intent(root.path(), command);
        assert_eq!(
            intent.resources.iter().any(|resource| resource.protected),
            expected
        );
    }

    #[test_case("/usr/bin/git status"; "absolute_executable")]
    #[test_case("./git status"; "relative_executable")]
    fn shell_preflight_preserves_the_executable_path(command: &str) {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": command}))
            .expect("valid shell input");

        let intent = smol::block_on(invocation.preflight(&ctx))
            .expect("shell preflight")
            .expect("shell permission intent");

        assert_eq!(intent.resources.len(), 1);
        assert_eq!(intent.resources[0].value, command);
        assert_eq!(
            intent.resources[0]
                .attributes
                .get(NORMALIZED_COMMAND_ATTRIBUTE)
                .map(String::as_str),
            Some("git status")
        );
    }

    #[test_case("git status > status.txt"; "trailing_redirect")]
    #[test_case("> status.txt git status"; "leading_redirect")]
    #[test_case("git status # '\n> victim"; "quote_in_comment")]
    fn shell_redirect_preflight_preserves_the_full_protected_command(command: &str) {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": command}))
            .expect("valid shell input");

        let intent = smol::block_on(invocation.preflight(&ctx))
            .expect("shell preflight")
            .expect("shell permission intent");

        assert!(!intent.scopes.force_prompt);
        assert_eq!(intent.authority, PermissionAuthorityProfile::Shell);
        let whole_source = intent.resources.last().expect("whole source resource");
        assert_eq!(whole_source.value, command);
        assert!(whole_source.protected);
        assert!(whole_source.requires_prompt);
        assert!(
            !whole_source
                .attributes
                .contains_key(NORMALIZED_COMMAND_ATTRIBUTE)
        );
    }

    /// Only the resource standing for the whole line says why the line could
    /// not be reviewed; the commands inside it, and a line that could be, say
    /// nothing.
    #[test_case("git status --short", None; "a_reviewable_line")]
    #[test_case("git status > status.txt", Some(ShellOpacity::Redirect); "a_redirect")]
    #[test_case("sudo ls", Some(ShellOpacity::Privilege); "privilege")]
    #[test_case("bash -c 'cargo test'", Some(ShellOpacity::InlineScript { language: ScriptLanguage::Shell }); "shell_code")]
    #[test_case("python3 -c 'print(1)'", Some(INLINE_PYTHON); "interpreter_code")]
    #[test_case("echo $(date)", Some(ShellOpacity::Dynamic); "a_substitution")]
    #[test_case("cat <<'EOF'\nnotes\nEOF\n", Some(ShellOpacity::Redirect); "a_heredoc")]
    fn only_the_whole_line_resource_names_its_opacity(
        command: &str,
        expected: Option<ShellOpacity>,
    ) {
        let root = TempDir::new().expect("tempdir");
        let intent = shell_preflight_intent(root.path(), command);
        let named: Vec<_> = intent
            .resources
            .iter()
            .filter_map(|resource| {
                ShellOpacity::of(resource).map(|opacity| {
                    (
                        resource.value.as_str(),
                        resource.protected && resource.requires_prompt,
                        opacity,
                    )
                })
            })
            .collect();
        assert_eq!(
            named,
            Vec::from_iter(expected.map(|opacity| (command, true, opacity)))
        );
    }

    /// Interpreter code makes the line ask as a whole, yet the command beside
    /// it is still exactly what runs, so pattern learning keeps observing it.
    #[test]
    fn interpreter_code_leaves_sibling_commands_observed() {
        let root = TempDir::new().expect("tempdir");
        let intent = shell_preflight_intent(root.path(), INTERPRETER_SIBLING);
        let whole_line = intent.resources.last().expect("whole line resource");
        assert_eq!(whole_line.value, INTERPRETER_SIBLING);
        assert_eq!(ShellOpacity::of(whole_line), Some(INLINE_PYTHON));
        let observed: Vec<_> = intent
            .resources
            .iter()
            .filter_map(|resource| resource.attributes.get(COMMAND_OBSERVATION_ATTRIBUTE))
            .map(|json| {
                CommandObservation::from_json(json)
                    .expect("observation")
                    .argv
            })
            .collect();
        assert_eq!(observed, [["cargo", "check", "-p", "core"]]);
    }

    /// The classifier's answer has to reach the permission layer or it only ever
    /// gated plan mode. This attribute is what the builtin allow rule keys on,
    /// so marking a line is the whole difference between running and asking.
    #[test_case("git status --short" => true ; "a read that cannot leave the project")]
    #[test_case("cat Cargo.toml" => true ; "a read of a relative path")]
    #[test_case("cat \"my file.txt\"" => true ; "a quoted operand is one word, not two")]
    #[test_case("find . -name '*.rs'" => true ; "a quoted glob is an ordinary argument")]
    #[test_case("sed -n '1,140p' Cargo.toml" => true ; "a sed script that only prints")]
    #[test_case("cd src && rg -n needle ." => true ; "a move into the project before reading")]
    #[test_case("git log --oneline -3 2>/dev/null | head -20" => true ; "a read that discards its stderr")]
    #[test_case("python3 --version" => true ; "a version probe")]
    #[test_case("command -v rg" => true ; "a name lookup")]
    #[test_case("type -t rg" => true ; "a lookup of what a name is")]
    #[test_case("cd /tmp && cat x" => false ; "a move out of it")]
    #[test_case("sed -i 's/a/b/' Cargo.toml" => false ; "a sed script that writes in place")]
    #[test_case("cat /etc/shadow" => false ; "a read that leaves the project")]
    #[test_case("cat ../../secret" => false ; "a read that climbs out of it")]
    #[test_case("rm -rf build" => false ; "not a read at all")]
    #[test_case("git status > out.txt" => false ; "an opaque line is never marked")]
    #[test_case("git \"-C\" /elsewhere log" => false ; "quoting does not hide a denied flag")]
    #[test_case("cat \\/etc/shadow" => false ; "escaping does not hide an absolute path")]
    // Every one of these names something its own text does not. Workcell marks
    // `${...}` and `$(...)` opaque but leaves the rest intact, so each arrived
    // at the confinement check looking like an ordinary relative path, and each
    // textual rule written to catch them missed the next one.
    #[test_case("cat $HOME/.ssh/id_rsa" => false ; "a bare variable")]
    #[test_case("cat \"$HOME\"/.ssh/id_rsa" => false ; "a quoted variable, which expands the same")]
    #[test_case("cat *" => false ; "an unquoted glob")]
    #[test_case("cat {/etc/shadow,x}" => false ; "a brace expansion naming an absolute path")]
    #[test_case("cat {1..9}" => false ; "a numeric brace range")]
    // A denied flag cannot be recognized in a word that cannot be read, and plan
    // mode gates on `is_read_only` alone, so the unreadable word has to
    // disqualify the line rather than wait for the confinement check.
    #[test_case("find . $FLAG" => false ; "find could be hiding -delete")]
    #[test_case("rg $PRE pattern" => false ; "ripgrep could be hiding --pre")]
    #[test_case("git branch -D topic" => false ; "branch_force_delete")]
    #[test_case("git branch --delete topic" => false ; "branch_delete")]
    #[test_case("git branch --del topic" => false ; "branch_abbreviated_delete")]
    #[test_case("git branch -m old new" => false ; "branch_rename")]
    #[test_case("git branch -c old new" => false ; "branch_copy")]
    #[test_case("git branch topic" => false ; "branch_create")]
    #[test_case("git branch --list -D topic" => false ; "branch_list_does_not_hide_mutation")]
    #[test_case("git tag release" => false ; "tag_create")]
    #[test_case("git tag -a release -m message" => false ; "tag_annotated_create")]
    #[test_case("git tag --list -d release" => false ; "tag_list_does_not_hide_delete")]
    #[test_case("git reflog expire --expire=now --all" => false ; "reflog_expire")]
    #[test_case("git reflog delete HEAD@{0}" => false ; "reflog_delete")]
    #[test_case("git reflog drop --all" => false ; "reflog_drop")]
    #[test_case("git branch --list 'topic*'" => true ; "branch_explicit_list")]
    #[test_case("git branch -avv" => false ; "unreviewed_branch_cluster")]
    #[test_case("git branch -a -vv" => true ; "branch_verbose_list")]
    #[test_case("git branch --show-current" => true ; "branch_current")]
    #[test_case("git tag" => true ; "tag_implicit_list")]
    #[test_case("git tag --list 'v*'" => true ; "tag_explicit_list")]
    #[test_case("git reflog show --oneline -3" => true ; "reflog_show")]
    #[test_case("git diff --out=output" => false ; "git_abbreviated_output")]
    #[test_case("git ls-files --open-files-in-pager=sh" => false ; "git_pager_helper")]
    #[test_case("git show --textconv HEAD" => false ; "git_textconv_helper")]
    #[test_case("git --paginate log" => false ; "git_explicit_pager")]
    #[test_case("./cat Cargo.toml" => false ; "relative_executable_spoof")]
    #[test_case("/usr/bin/cat Cargo.toml" => false ; "absolute_executable_unproven")]
    #[test_case("'./cat' Cargo.toml" => false ; "quoted_executable_spoof")]
    #[test_case("./git status" => false ; "git_executable_spoof")]
    #[test_case("./cd . && cat Cargo.toml" => false ; "cd_executable_spoof")]
    #[test_case("'cat' Cargo.toml" => false ; "quoted_executable_unproven")]
    #[test_case("c\\at Cargo.toml" => false ; "escaped_executable_unproven")]
    #[test_case("cat .env" => false ; "protected_dotenv")]
    #[test_case("cat .env.local" => false ; "protected_dotenv_variant")]
    #[test_case("cat .git/config" => false ; "protected_git_config")]
    #[test_case("cat .git/hooks/pre-commit" => false ; "protected_git_hook")]
    #[test_case("cat .ssh/id_rsa" => false ; "protected_ssh")]
    #[test_case("cat .aws/credentials" => false ; "protected_aws")]
    #[test_case("cat vendor/dep/.git/HEAD" => false ; "protected_nested_git")]
    #[test_case("cat .git/HEAD" => true ; "inert_project_git_head")]
    #[test_case("cat .git/refs/heads/main" => true ; "inert_project_git_ref")]
    #[test_case("rg --file=.env needle src" => false ; "protected_attached_long_operand")]
    #[test_case("grep -f.env Cargo.toml" => false ; "protected_attached_short_operand")]
    #[test_case("rg -f.env Cargo.toml" => false ; "protected_ripgrep_attached_operand")]
    #[test_case("git show HEAD:.env" => false ; "protected_revision_operand")]
    #[test_case("cd .git && cat config" => false ; "protected_directory_change")]
    #[test_case("cd -P && cat x" => false ; "implicit_home_after_cd_option")]
    #[test_case("date -s now" => false ; "date_set")]
    #[test_case("date --se=now" => false ; "date_abbreviated_set")]
    #[test_case("date 010100002026" => false ; "date_positional_set")]
    #[test_case("date -u +%F" => true ; "date_display")]
    #[test_case("tree -o output" => false ; "tree_output")]
    #[test_case("tree -a src" => true ; "tree_listing")]
    #[test_case("file -C -m magic" => false ; "file_compile")]
    #[test_case("file -z archive.gz" => false ; "file_decompress_helper")]
    #[test_case("file --mime-type Cargo.toml" => true ; "file_identify")]
    #[test_case("jq --run-tests tests.jq" => false ; "jq_unreviewed_mode")]
    #[test_case("printf -v PATH value" => false ; "printf_variable_assignment")]
    #[test_case("find . -fprint0 output" => false ; "find_null_output")]
    #[test_case("find -L . -name '*.rs'" => false ; "find_follow_links")]
    #[test_case("rg --follow needle ." => false ; "ripgrep_follow_links")]
    #[test_case("sort --out=output Cargo.toml" => false ; "sort_abbreviated_output")]
    #[test_case("sort -nu Cargo.toml" => true ; "sort_numeric_unique")]
    #[test_case("grep --dereference-r needle ." => false ; "grep_abbreviated_follow")]
    #[test_case("wc --files0-from=paths" => false ; "wc_indirect_file_operands")]
    #[test_case("du --files0-f=paths" => false ; "du_abbreviated_indirect_operands")]
    fn shell_preflight_marks_only_a_confined_read(command: &str) -> bool {
        let root = TempDir::new().expect("tempdir");
        confined_read_preflight_marks(root.path(), command)
    }

    /// A checked-in symlink needs no privileged action from the model: cloning a
    /// repository is enough. Every textual rule says `cat notes.md` is confined,
    /// so only resolving it says otherwise.
    #[test_case("cat inside.md" => true ; "a real file inside the project")]
    #[test_case("cat notes.md" => false ; "a symlink out of the project")]
    #[test_case("cat linked/id_rsa" => false ; "a path through a symlinked directory")]
    fn shell_preflight_resolves_a_symlink_before_marking(command: &str) -> bool {
        let root = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        std::fs::write(outside.path().join("id_rsa"), "key").expect("secret");
        std::fs::write(root.path().join("inside.md"), "notes").expect("inside");
        std::os::unix::fs::symlink(outside.path().join("id_rsa"), root.path().join("notes.md"))
            .expect("file link");
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).expect("dir link");

        confined_read_preflight_marks(root.path(), command)
    }

    #[test_case(".env", false; "dotenv_alias")]
    #[test_case(".git/config", false; "git_config_alias")]
    #[test_case(".git/HEAD", true; "inert_git_alias")]
    #[test_case("notes.md", true; "ordinary_alias")]
    fn shell_preflight_protects_resolved_operands(target: &str, expected: bool) {
        let root = TempDir::new().expect("tempdir");
        let path = root.path().join(target);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent directory");
        std::fs::write(&path, "fixture").expect("target");
        std::os::unix::fs::symlink(&path, root.path().join("alias")).expect("alias");

        assert_eq!(
            confined_read_preflight_marks(root.path(), "cat alias"),
            expected
        );
    }

    #[test_case("execution_environment")]
    fn environment_preflight_keeps_explicit_authority(tool: &str) {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get(tool)
            .expect("registered tool")
            .tool
            .parse(&json!({}))
            .expect("valid input");
        let intent = smol::block_on(invocation.preflight(&ctx))
            .expect("preflight")
            .expect("permission intent");

        assert_eq!(intent.resources.len(), 1);
        assert!(matches!(
            intent.resources[0].kind,
            PermissionResourceKind::Custom { .. }
        ));
        assert_eq!(intent.resources[0].value, tool);
        assert!(
            !intent.resources[0]
                .attributes
                .contains_key(CONFINED_READ_ATTRIBUTE)
        );
        assert!(!matches!(
            invocation.plan_mode_access(),
            PlanModeAccess::ReadOnly
        ));
    }

    /// Plan mode gates on `is_read_only` alone, with no confinement check, so
    /// the classifier has to be self-sufficient there. A denied flag cannot be
    /// recognized inside a word the parse could not read, which is why an
    /// unreadable word disqualifies the line rather than deferring to a check
    /// this path never runs.
    #[test_case("git log --oneline" => true ; "a read the parse can account for")]
    #[test_case("find . -name '*.rs'" => true ; "a quoted glob is an ordinary argument")]
    #[test_case("cat /etc/shadow" => true ; "plan mode judges the verb, not the operand")]
    #[test_case("find . $FLAG" => false ; "find could be hiding -delete")]
    #[test_case("rg $PRE pattern" => false ; "ripgrep could be hiding --pre")]
    #[test_case("git -c core.pager=sh log" => false ; "a flag it can read is refused on its merits")]
    #[test_case("rm -rf build" => false ; "not a read at all")]
    #[test_case("git branch -D topic" => false ; "branch_force_delete")]
    #[test_case("git branch --del topic" => false ; "branch_abbreviated_delete")]
    #[test_case("git branch topic" => false ; "branch_create")]
    #[test_case("git tag release" => false ; "tag_create")]
    #[test_case("git tag --list -d release" => false ; "tag_list_with_delete")]
    #[test_case("git reflog expire --expire=now --all" => false ; "reflog_expire")]
    #[test_case("git reflog delete HEAD@{0}" => false ; "reflog_delete")]
    #[test_case("git reflog drop --all" => false ; "reflog_drop")]
    #[test_case("git branch -a -vv" => true ; "branch_listing")]
    #[test_case("git tag --list 'v*'" => true ; "tag_listing")]
    #[test_case("git reflog show --oneline -3" => true ; "reflog_show")]
    #[test_case("git diff --out=output" => false ; "git_abbreviated_output")]
    #[test_case("git ls-files --open-files-in-pager=sh" => false ; "git_pager_helper")]
    #[test_case("git show --textconv HEAD" => false ; "git_textconv_helper")]
    #[test_case("./cat Cargo.toml" => false ; "relative_executable_spoof")]
    #[test_case("/usr/bin/cat Cargo.toml" => false ; "absolute_executable_unproven")]
    #[test_case("'./cat' Cargo.toml" => false ; "quoted_executable_spoof")]
    #[test_case("./cd ." => false ; "cd_executable_spoof")]
    #[test_case("cat .env" => true ; "protected_read_still_requires_permission")]
    #[test_case("cat .git/config" => true ; "protected_git_read_still_requires_permission")]
    #[test_case("date -s now" => false ; "date_set")]
    #[test_case("date --se=now" => false ; "date_abbreviated_set")]
    #[test_case("date 010100002026" => false ; "date_positional_set")]
    #[test_case("date -u +%F" => true ; "date_display")]
    #[test_case("tree -o output" => false ; "tree_output")]
    #[test_case("file -C -m magic" => false ; "file_compile")]
    #[test_case("file -z archive.gz" => false ; "file_decompress_helper")]
    #[test_case("jq --run-tests tests.jq" => false ; "jq_unreviewed_mode")]
    #[test_case("printf -v PATH value" => false ; "printf_variable_assignment")]
    #[test_case("find . -fprint0 output" => false ; "find_null_output")]
    #[test_case("sort --out=output Cargo.toml" => false ; "sort_abbreviated_output")]
    fn plan_mode_admits_only_a_line_it_could_read(command: &str) -> bool {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": command}))
            .expect("valid shell input");

        smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");

        matches!(invocation.plan_mode_access(), PlanModeAccess::ReadOnly)
    }

    fn shell_redirect_preflight(
        command: &str,
        redirect: ShellNativeRedirect,
        workdir_redirect: bool,
    ) -> Result<(), ToolError> {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        ctx.config.shell_native_redirect = redirect;
        ctx.config.shell_workdir_redirect = workdir_redirect;
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": command}))
            .expect("valid shell input");

        smol::block_on(invocation.preflight(&ctx)).map(|_| ())
    }

    const DUPLICATING_COMMAND: &str = "rg needle src";
    const IRREPLACEABLE_COMMAND: &str = "rg -i needle src";

    #[test_case(ShellNativeRedirect::Annotate ; "annotating only observes")]
    #[test_case(ShellNativeRedirect::Off ; "the check is disabled")]
    fn a_duplicating_command_still_runs_outside_enforcement(redirect: ShellNativeRedirect) {
        assert!(shell_redirect_preflight(DUPLICATING_COMMAND, redirect, false).is_ok());
    }

    #[test]
    fn enforcement_refuses_a_duplicating_command_and_names_the_tool() {
        let error =
            shell_redirect_preflight(DUPLICATING_COMMAND, ShellNativeRedirect::Enforce, false)
                .expect_err("enforcement must refuse");

        assert!(
            error.message.contains("file_grep"),
            "{error:?} must name the tool"
        );
        assert_eq!(error.failure, ToolFailure::Denied);
    }

    /// The trap enforcement has to avoid: a flag the native tool cannot express
    /// leaves the model with nowhere to go if the shell is closed to it too.
    #[test]
    fn enforcement_leaves_a_search_the_native_tool_cannot_express() {
        assert!(
            shell_redirect_preflight(IRREPLACEABLE_COMMAND, ShellNativeRedirect::Enforce, false)
                .is_ok()
        );
    }

    #[test_case(ShellNativeRedirect::Enforce; "native_enforced")]
    #[test_case(ShellNativeRedirect::Annotate; "native_annotated")]
    #[test_case(ShellNativeRedirect::Off; "native_disabled")]
    fn workdir_redirect_is_independent_of_native_redirect(redirect: ShellNativeRedirect) {
        let error = shell_redirect_preflight(LEADING_CD_COMMAND, redirect, true)
            .expect_err("leading cd must be refused");

        assert_eq!(error.failure, ToolFailure::Denied);
        assert_eq!(error.message, native_redirect::WORKDIR_REFUSAL);
        assert!(shell_redirect_preflight(LEADING_CD_COMMAND, redirect, false).is_ok());
    }

    #[test]
    fn workdir_redirect_takes_precedence_over_native_redirect() {
        let error =
            shell_redirect_preflight("cd src && rg needle", ShellNativeRedirect::Enforce, true)
                .expect_err("leading cd must be refused");

        assert_eq!(error.message, native_redirect::WORKDIR_REFUSAL);
    }

    #[test_case("cd src && cd - && cargo test"; "previous_directory")]
    #[test_case("cd src && printf '%s' \"$OLDPWD\""; "previous_directory_expansion")]
    #[test_case("cd link && cd .. && cargo test"; "logical_parent_directory")]
    fn workdir_redirect_preserves_directory_state_commands(command: &str) {
        assert!(shell_redirect_preflight(command, ShellNativeRedirect::Enforce, true).is_ok());
    }

    #[test_case("nested"; "inside_project")]
    #[test_case("../../workcell-mcp"; "sibling_project")]
    fn workdir_redirect_accepts_retry_from_an_existing_workdir(target: &str) {
        let root = TempDir::new().expect("tempdir");
        let project = root.path().join("project");
        let initial = project.join("initial");
        let destination = initial.join(target);
        fs::create_dir_all(&initial).expect("initial directory");
        fs::create_dir_all(&destination).expect("destination directory");
        let (_host, registry) = host_and_registry(&project);
        let mut ctx = context(&project, Arc::clone(&registry), CancelToken::none());
        ctx.config.shell_workdir_redirect = true;
        ctx.config.shell_native_redirect = ShellNativeRedirect::Enforce;
        let shell = registry.get("shell").expect("registered shell");
        let invocation = shell
            .tool
            .parse(&json!({"command": format!("cd {target} && cargo test"), "workdir": "initial"}))
            .expect("valid shell input");
        let error = smol::block_on(invocation.preflight(&ctx)).expect_err("leading cd refused");
        assert_eq!(error.message, native_redirect::WORKDIR_REFUSAL);

        let retry = shell
            .tool
            .parse(&json!({"command": "cargo test", "workdir": destination}))
            .expect("valid retry");
        smol::block_on(retry.preflight(&ctx)).expect("retry accepts workdir");
    }

    fn shell_preflight_intent(root: &Path, command: &str) -> PermissionIntent {
        let (_host, registry) = host_and_registry(root);
        let ctx = context(root, Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": command}))
            .expect("valid shell input");

        smol::block_on(invocation.preflight(&ctx))
            .expect("shell preflight")
            .expect("shell permission intent")
    }

    /// The effect a read-only agent is judged on. It exists only once preflight
    /// has parsed the line: before that the invocation cannot tell a confined
    /// read from a build, so it keeps the registered worst case.
    fn shell_call_effect(root: &Path, command: &str, preflight: bool) -> ToolEffect {
        let (_host, registry) = host_and_registry(root);
        let ctx = context(root, Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": command}))
            .expect("valid shell input");
        if preflight {
            smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");
        }
        invocation.call_effect(ToolEffect::Mutating)
    }

    #[test_case("rg needle src", true => ToolEffect::ReadOnly ; "a confined read")]
    #[test_case("cargo build", true => ToolEffect::Mutating ; "a command that executes")]
    #[test_case("cat /etc/passwd", true => ToolEffect::Mutating ; "a read outside the project")]
    #[test_case("rg needle src && cargo build", true => ToolEffect::Mutating ; "one command short")]
    #[test_case("$UNREADABLE", true => ToolEffect::Mutating ; "a line nobody could parse")]
    #[test_case("rg needle src", false => ToolEffect::Mutating ; "a line nobody has parsed")]
    fn a_shell_line_is_read_only_only_when_every_command_is_a_confined_read(
        command: &str,
        preflight: bool,
    ) -> ToolEffect {
        let root = TempDir::new().expect("tempdir");
        shell_call_effect(root.path(), command, preflight)
    }

    const RECORDED_FILE: &str = "notes.md";
    const REMOTE_SUBDIRECTORY: &str = "sub";

    fn recorded(paths: &[&str]) -> Option<RecordScope> {
        Some(RecordScope::Paths(
            paths
                .iter()
                .map(|path| caudra_workspace::WorkspacePath::new(*path).expect("workspace path"))
                .collect(),
        ))
    }

    fn too_deep_to_parse() -> String {
        let depth = MAX_BASH_DEPTH + 1;
        format!(
            "{}rm {RECORDED_FILE}{}",
            "( ".repeat(depth),
            " )".repeat(depth)
        )
    }

    #[test_case(SHELL_TOOL_NAME, json!({"command": "rm notes.md"}), true => recorded(&[RECORDED_FILE]) ; "a_shell_line_records_what_it_names")]
    #[test_case(SHELL_TOOL_NAME, json!({"command": "ls /tmp"}), true => None ; "a_shell_read_records_nothing")]
    #[test_case(SHELL_TOOL_NAME, json!({"command": "cargo fmt"}), true => Some(RecordScope::Workspace) ; "an_opaque_line_records_everything")]
    #[test_case(SHELL_TOOL_NAME, json!({"command": "rm notes.md"}), false => Some(RecordScope::Workspace) ; "an_unprepared_line_records_everything")]
    #[test_case(SHELL_TOOL_NAME, json!({"command": too_deep_to_parse()}), true => Some(RecordScope::Workspace) ; "an_unparsed_line_records_everything")]
    #[test_case("file_write", json!({"filePath": RECORDED_FILE, "content": ""}), true => recorded(&[RECORDED_FILE]) ; "a_file_tool_records_its_target")]
    fn a_call_records_what_it_names(
        tool: &str,
        input: Value,
        preflight: bool,
    ) -> Option<RecordScope> {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical root");
        let (_host, registry) = host_and_registry(&root);
        let ctx = context(&root, Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get(tool)
            .expect("registered tool")
            .tool
            .parse(&input)
            .expect("valid input");
        if preflight {
            smol::block_on(invocation.preflight(&ctx)).expect("preflight");
        }
        invocation.record_scope(&ctx, &root)
    }

    #[test_case(REMOTE_SUBDIRECTORY, "rm x" => recorded(&["sub/x"]) ; "a_target_lands_under_the_cursor")]
    #[test_case(CURRENT_WORKDIR, "rm /workspace/x" => Some(RecordScope::Workspace) ; "an_absolute_target_is_unplaced")]
    #[test_case(CURRENT_WORKDIR, "ls /tmp" => None ; "a_read_records_nothing")]
    #[test_case(CURRENT_WORKDIR, "cargo fmt" => Some(RecordScope::Workspace) ; "an_opaque_line_records_everything")]
    #[test_case(CURRENT_WORKDIR, &too_deep_to_parse() => Some(RecordScope::Workspace) ; "an_unparsed_line_records_everything")]
    fn a_remote_line_records_what_it_names_under_its_cursor(
        cwd: &str,
        command: &str,
    ) -> Option<RecordScope> {
        let startup = serde_json::to_string(&REMOTE_SHELL_ASSUMPTIONS).expect("assumptions");
        let resources: Vec<host_contract::ResourceIntent> = serde_json::from_value(json!([
            {"resourceId": "cwd", "scope": ["cwd"], "display": cwd, "access": "traverse", "revision": null},
            {"resourceId": "command", "scope": ["command"], "display": command, "access": "execute", "revision": null},
            {"resourceId": "startup", "scope": ["startup"], "display": startup, "access": "inspect", "revision": null},
        ]))
        .expect("resource intents");
        remote_shell_scope(&resources)
    }

    #[test]
    fn a_remote_line_in_an_unreviewed_shape_records_everything() {
        assert_eq!(remote_shell_scope(&[]), Some(RecordScope::Workspace));
    }

    fn confined_read_preflight_rows(root: &Path, command: &str) -> Vec<bool> {
        shell_preflight_intent(root, command)
            .resources
            .iter()
            .map(|resource| {
                resource
                    .attributes
                    .get(CONFINED_READ_ATTRIBUTE)
                    .map(String::as_str)
                    == Some(CONFINED_READ_VALUE)
            })
            .collect()
    }

    fn confined_read_preflight_marks(root: &Path, command: &str) -> bool {
        let rows = confined_read_preflight_rows(root, command);
        !rows.is_empty() && rows.iter().all(|resource| *resource)
    }

    /// The attribute is what the builtin rule turns on, and it is set per
    /// resource, so only a call through the real preflight shows that a command
    /// which executes stops tainting the observers beside it. Every row was
    /// unmarked here until the classifier answered per command, which cost a
    /// prompt for the `cd` and the `rg` even when the `cargo` was allowed.
    #[test]
    fn a_preflight_marks_only_the_commands_that_observe() {
        let root = TempDir::new().expect("tempdir");

        assert_eq!(
            confined_read_preflight_rows(root.path(), "cd src && cargo check && rg needle src"),
            vec![true, false, true]
        );
    }

    #[test]
    fn shell_descriptor_duplication_stays_reviewable() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": "cargo check --all-targets 2>&1"}))
            .expect("valid shell input");

        let intent = smol::block_on(invocation.preflight(&ctx))
            .expect("shell preflight")
            .expect("shell permission intent");

        assert_eq!(intent.resources.len(), 1);
        assert_eq!(intent.resources[0].value, "cargo check --all-targets");
        assert!(!intent.resources[0].protected);
        assert!(!intent.resources[0].requires_prompt);
        assert!(
            !intent.resources[0]
                .attributes
                .contains_key(COMMAND_OBSERVATION_ATTRIBUTE)
        );
    }

    #[test_case("left/note"; "left_success")]
    #[test_case("right/note"; "right_success_is_not_left_slash_right")]
    #[test_case("note"; "both_changes_fail")]
    fn shell_preflight_checks_every_possible_cd_branch(link: &str) {
        let root = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        std::fs::create_dir_all(root.path().join("left/right")).expect("left directory");
        std::fs::create_dir(root.path().join("right")).expect("right directory");
        std::fs::write(outside.path().join("note"), "outside").expect("outside file");
        std::os::unix::fs::symlink(outside.path().join("note"), root.path().join(link))
            .expect("escape link");

        assert_eq!(
            confined_read_preflight_rows(root.path(), "cd left || cd right; cat note"),
            vec![true, true, false]
        );
    }

    #[test_case("notes.txt", true; "all_branches_inside")]
    #[test_case(".env", false; "one_branch_resolves_to_protected_file")]
    fn shell_preflight_applies_protection_to_every_incoming_directory(
        target: &str,
        expected: bool,
    ) {
        let root = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("left/right")).expect("left directory");
        std::fs::create_dir(root.path().join("right")).expect("right directory");
        std::fs::write(root.path().join(target), "fixture").expect("target");
        std::os::unix::fs::symlink(root.path().join(target), root.path().join("right/note"))
            .expect("link");
        assert_eq!(
            confined_read_preflight_rows(root.path(), "cd left || cd right; cat note"),
            vec![true, true, expected]
        );
    }

    #[test]
    fn shell_preflight_keeps_the_initial_directory_after_a_failed_cd() {
        let root = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        std::fs::write(outside.path().join("note"), "outside").expect("outside file");
        std::os::unix::fs::symlink(outside.path().join("note"), root.path().join("note"))
            .expect("escape link");
        assert_eq!(
            confined_read_preflight_rows(root.path(), "cd missing; cat note"),
            vec![true, false]
        );
    }

    #[test_case("cd left | cat note; cat note", vec![true, true, true]; "pipeline_isolation")]
    #[test_case("cd left && cat note", vec![true, false]; "and_success")]
    #[test_case("(cd left && cat note); cat note", vec![true, false, true]; "subshell_isolation")]
    fn shell_preflight_tracks_control_flow_not_source_order(command: &str, expected: Vec<bool>) {
        let root = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        std::fs::create_dir(root.path().join("left")).expect("left directory");
        std::fs::write(outside.path().join("note"), "outside").expect("outside file");
        std::os::unix::fs::symlink(outside.path().join("note"), root.path().join("left/note"))
            .expect("escape link");
        assert_eq!(confined_read_preflight_rows(root.path(), command), expected);
    }

    #[test_case("cargo check &&"; "incomplete_syntax")]
    #[test_case("cargo check <<-EOF\n\t$(touch hidden)\n\tEOF\n"; "heredoc_dash_byte_gap")]
    #[test_case("MODE=$(echo hidden) cargo check"; "environment_substitution")]
    #[test_case("cargo check >$(echo hidden)"; "redirect_substitution")]
    #[test_case("if true; then cargo check; fi"; "unsupported_conditional")]
    #[test_case("f() { cargo check; }; f"; "function_body")]
    #[test_case("bash -c 'cargo check'"; "interpreter_wrapper")]
    #[test_case("cd . && cargo check"; "unknown_cwd_effect")]
    #[test_case("hash -p ./cargo cargo; cargo check"; "builtin_command_resolution_state")]
    #[test_case("pwd -P; cargo check"; "builtin_flags_need_positive_classification")]
    #[test_case("umask 000; cargo check"; "builtin_process_state")]
    fn shell_preflight_requires_exact_source_for_unaccounted_effects(command: &str) {
        let root = TempDir::new().expect("tempdir");
        let intent = shell_preflight_intent(root.path(), command);
        let fallback = intent.resources.last().expect("fallback resource");
        assert_eq!(fallback.value, command);
        assert!(fallback.protected && fallback.requires_prompt);
        assert!(intent.resources.iter().all(|resource| {
            !resource
                .attributes
                .contains_key(COMMAND_OBSERVATION_ATTRIBUTE)
                && !resource.attributes.contains_key(CONFINED_READ_ATTRIBUTE)
        }));
    }

    #[test_case("cargo check -p core", "."; "direct_command")]
    #[test_case("cd crate && cargo check -p core", "crate"; "successful_cd")]
    fn shell_preflight_observes_the_effective_command_workdir(command: &str, directory: &str) {
        let root = TempDir::new().expect("tempdir");
        let intent = shell_preflight_intent(root.path(), command);
        let resource = intent.resources.last().expect("cargo resource");
        let observation = CommandObservation::from_json(
            resource
                .attributes
                .get(COMMAND_OBSERVATION_ATTRIBUTE)
                .expect("observation"),
        )
        .expect("valid observation");
        assert_eq!(observation.argv, ["cargo", "check", "-p", "core"]);
        assert_eq!(observation.source.provenance, ObservationProvenance::Native);
        assert_eq!(
            observation.verification.shell_effects,
            ShellEffectStatus::Absent
        );
        assert_eq!(
            Path::new(&observation.context.effective_workdir),
            root.path()
                .canonicalize()
                .expect("canonical root")
                .join(directory)
        );
        assert_eq!(
            resource.attributes.get("workdir"),
            Some(&observation.context.effective_workdir)
        );
        assert!(intent.resources.iter().all(|resource| !resource.protected));
    }

    #[test]
    fn shell_preflight_binds_identical_commands_to_their_source_spans() {
        let root = TempDir::new().expect("tempdir");
        let intent = shell_preflight_intent(
            root.path(),
            "(cd left && cargo check -p core); (cd right && cargo check -p core)",
        );
        let observations = intent
            .resources
            .iter()
            .filter_map(|resource| {
                resource
                    .attributes
                    .get(COMMAND_OBSERVATION_ATTRIBUTE)
                    .map(|json| CommandObservation::from_json(json).expect("observation"))
            })
            .collect::<Vec<_>>();
        assert_eq!(observations.len(), 2);
        assert!(observations[0].context.effective_workdir.ends_with("/left"));
        assert!(
            observations[1]
                .context
                .effective_workdir
                .ends_with("/right")
        );
        assert_ne!(
            observations[0].source.observation_id,
            observations[1].source.observation_id
        );
    }

    #[test_case("cargo check -p core 2>&1"; "descriptor")]
    #[test_case("cargo check -p core --password redacted"; "sensitive")]
    #[test_case("python3 -c 'print(1)'"; "payload")]
    #[test_case("./cargo check -p core"; "executable_script")]
    #[test_case("cd left; cargo check -p core"; "ambiguous_context")]
    #[test_case("novelctl inspect -c body"; "generic_code_flag")]
    #[test_case("novelctl inspect -e body"; "generic_expression_flag")]
    #[test_case("novelctl inspect -f code"; "generic_script_file")]
    #[test_case("novelctl inspect --eval body"; "generic_eval_flag")]
    #[test_case("novelctl inspect --script code"; "generic_script_flag")]
    #[test_case("novelctl inspect --command body"; "generic_command_flag")]
    #[test_case("novelctl inspect --config settings"; "generic_config_flag")]
    #[test_case("novelctl inspect @arguments.rsp"; "generic_response_file")]
    #[test_case("python3.13 --version"; "versioned_interpreter")]
    #[test_case("node --version"; "javascript_interpreter")]
    #[test_case("perl -v"; "perl_interpreter")]
    #[test_case("awk 'BEGIN { print 1 }'"; "awk_program")]
    #[test_case("sed -n '1p' file"; "sed_program")]
    #[test_case("ssh host true"; "remote_shell_payload")]
    #[test_case("sudo novelctl inspect --name alpha"; "privilege_wrapper")]
    #[test_case("env novelctl inspect --name alpha"; "environment_wrapper")]
    fn shell_preflight_does_not_attach_ineligible_observations(command: &str) {
        let root = TempDir::new().expect("tempdir");
        let intent = shell_preflight_intent(root.path(), command);
        assert!(intent.resources.iter().all(|resource| {
            !resource
                .attributes
                .contains_key(COMMAND_OBSERVATION_ATTRIBUTE)
        }));
    }

    async fn prepared_shell_request(
        root: &Path,
        registry: &Arc<ToolRegistry>,
        input: Value,
        id: &str,
    ) -> PermissionRequest {
        let ctx = context(root, Arc::clone(registry), CancelToken::none());
        let registered = registry.get("shell").expect("shell tool");
        let invocation = registered.tool.parse(&input).expect("shell input");
        let timeout = effective_timeout(SHELL_TOOL_NAME, &input);
        assert_eq!(invocation.shell_timeout(), timeout);
        assert_eq!(invocation.permission_input(), Some(&input));
        let intent = invocation
            .preflight(&ctx)
            .await
            .expect("preflight")
            .expect("intent");
        assert_eq!(invocation.shell_timeout(), timeout);
        assert_eq!(invocation.permission_input(), Some(&input));
        let ToolSource::Native {
            owner, contract, ..
        } = &registered.source
        else {
            panic!("native shell source")
        };
        PermissionRequest::from_intent_with_identity(
            id.into(),
            ToolKey::native("shell"),
            &intent,
            input,
            root,
            PermissionSubject::Native {
                owner: owner.to_string(),
                contract: contract.to_string(),
            },
            PermissionExecutorKind::Native,
        )
    }

    async fn enforce_shell_request(
        manager: &PermissionManager,
        request: &PermissionRequest,
        events: &EventSender,
    ) -> Result<(), PermissionError> {
        let (_sender, receiver) = flume::unbounded();
        let responses = AsyncMutex::new(receiver);
        let intent = PermissionIntent::new(
            PermissionScopes {
                scopes: request.scopes.clone(),
                force_prompt: false,
                plan_scoped: false,
            },
            request.resources.clone(),
            request.risk.clone(),
        )
        .with_authority(PermissionAuthorityProfile::Shell);
        manager
            .enforce_with_intent(
                &request.tool,
                &intent,
                &request.input,
                events,
                Some(&responses),
                &request.id,
                &CancelToken::none(),
                None,
                Some((request.subject.clone(), request.executor.clone())),
                true,
            )
            .await
    }

    #[test_case(json!({"command": PATTERN_COMMAND}); "omitted_defaults")]
    #[test_case(json!({"command": PATTERN_COMMAND, "workdir": "."}); "explicit_workdir")]
    #[test_case(json!({"command": PATTERN_COMMAND, "timeoutSec": PATTERN_TIMEOUT_SECS}); "explicit_timeout")]
    #[test_case(json!({"command": PATTERN_COMMAND, "workdir": null, "timeoutSec": null}); "explicit_nulls")]
    #[test_case(json!({"command": "cd crate && cargo check -p alpha --tests", "workdir": "."}); "primitive_in_compound_source")]
    fn shell_preflight_binds_original_json_not_typed_defaults(input: Value) {
        smol::block_on(async {
            let root = TempDir::new().expect("project");
            let (_host, registry) = host_and_registry(root.path());
            let request =
                prepared_shell_request(root.path(), &registry, input.clone(), "binding").await;
            assert_eq!(request.input, input);
            let resource = request.resources.last().expect("cargo resource");
            let fact =
                CommandObservation::from_json(&resource.attributes[COMMAND_OBSERVATION_ATTRIBUTE])
                    .expect("native observation");
            let binding = prepared_command_binding(&resource.value, &input);
            assert_eq!(
                resource.attributes[COMMAND_OBSERVATION_BINDING_ATTRIBUTE],
                binding
            );
            assert_eq!(fact.source.input_hash, binding);
            let wire = serde_json::to_value(&request).expect("wire request");
            let restored: PermissionRequest =
                serde_json::from_value(wire.clone()).expect("restored request");
            for resource in wire["resources"].as_array().expect("wire resources") {
                assert!(
                    resource["attributes"]
                        .get(COMMAND_OBSERVATION_ATTRIBUTE)
                        .is_none()
                );
                assert!(
                    resource["attributes"]
                        .get(COMMAND_OBSERVATION_BINDING_ATTRIBUTE)
                        .is_none()
                );
            }
            assert!(restored.resources.iter().all(|resource| {
                !resource
                    .attributes
                    .contains_key(COMMAND_OBSERVATION_ATTRIBUTE)
                    && !resource
                        .attributes
                        .contains_key(COMMAND_OBSERVATION_BINDING_ATTRIBUTE)
            }));
        });
    }

    #[test_case(None; "missing_original_json")]
    #[test_case(Some(json!({})); "missing_original_command")]
    #[test_case(Some(json!({"command": "cargo clean"})); "mismatched_original_command")]
    fn shell_preflight_drops_facts_without_an_input_binding(raw_input: Option<Value>) {
        let root = TempDir::new().expect("project");
        let (host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), registry, CancelToken::none());
        let invocation = WorkcellInvocation {
            host: Arc::clone(&host.inner),
            input: Input::parse(ToolKind::Shell, json!({"command": PATTERN_COMMAND}))
                .expect("shell input"),
            raw_input,
            prepared: Mutex::new(None),
        };
        let intent = smol::block_on(invocation.prepare(&ctx)).expect("preflight");
        assert!(intent.resources.iter().all(|resource| {
            !resource
                .attributes
                .contains_key(COMMAND_OBSERVATION_ATTRIBUTE)
                && !resource
                    .attributes
                    .contains_key(COMMAND_OBSERVATION_BINDING_ATTRIBUTE)
        }));
    }

    #[test_case(false; "direct_command")]
    #[test_case(true; "successful_cd_composition")]
    fn shell_preflight_live_templates_compose_persist_reload_and_match(compound: bool) {
        smol::block_on(async {
            let root = TempDir::new().expect("project");
            std::fs::create_dir(root.path().join("crate")).expect("crate workdir");
            let state_root = TempDir::new().expect("permission state");
            let state = StateDir::from_path(state_root.path().to_path_buf());
            let manager = PermissionManager::new_persistent_in(
                PermissionsConfig::default(),
                root.path().to_path_buf(),
                Arc::default(),
                state.clone(),
            );
            let (_host, registry) = host_and_registry(root.path());
            let input_for = |package: &str| {
                json!({
                    "command": format!("{}cargo check -p {package} --tests", if compound { "cd crate && " } else { "" }),
                })
            };
            let cargo_index = usize::from(compound);
            for (index, package) in PATTERN_PACKAGES.iter().enumerate() {
                let request =
                    prepared_shell_request(root.path(), &registry, input_for(package), package)
                        .await;
                assert!(
                    request.resources[cargo_index]
                        .attributes
                        .contains_key(POSSIBLE_WORKDIRS_ATTRIBUTE)
                );
                let (events, received) = flume::unbounded();
                let events = EventSender::new(events, 0);
                let mut enforcement = Box::pin(enforce_shell_request(&manager, &request, &events));
                assert!(future::poll_once(&mut enforcement).await.is_none());
                let AgentEvent::PermissionRequest(offered) =
                    received.try_recv().expect("prompt").event
                else {
                    panic!("permission request")
                };
                assert_eq!(offered.input, request.input);
                assert!(
                    offered.resources[cargo_index]
                        .attributes
                        .contains_key(COMMAND_OBSERVATION_BINDING_ATTRIBUTE)
                );
                let exact = offered
                    .options
                    .iter()
                    .find(|option| option.id == format!("{COMMAND_EXACT_PREFIX}{cargo_index}"))
                    .expect("exact command option");
                let review = review_for_rule(&offered, &exact.rule);
                assert!(
                    review.resources[0]
                        .value
                        .as_ref()
                        .expect("command label")
                        .contains("cargo check")
                );
                assert!(
                    review.resources[0].attributes[POSSIBLE_WORKDIRS_ATTRIBUTE]
                        .starts_with(POSSIBLE_WORKDIRS_LABEL)
                );
                let template = offered
                    .options
                    .iter()
                    .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX));
                let answer = if index + 1 == PATTERN_PACKAGES.len() {
                    let template = template.expect("candidate from three real preflights");
                    let PermissionResourceSelector::CommandTemplate { definition } =
                        &template.rule.resources[0].selector
                    else {
                        panic!("learned template selector")
                    };
                    assert_eq!(definition.slots.len(), 1);
                    assert_eq!(
                        definition.slots[0].domain,
                        ArgumentDomain::ObservedSet {
                            values: PATTERN_PACKAGES
                                .iter()
                                .map(|package| (*package).into())
                                .collect(),
                        }
                    );
                    assert!(template.label.contains("cargo"));
                    assert!(template.label.contains("<pattern1>"));
                    assert!(
                        template
                            .description
                            .contains(&format!("{} observations", PATTERN_PACKAGES.len()))
                    );
                    assert_eq!(
                        template.group.as_ref().and_then(|group| group.resource),
                        Some(cargo_index)
                    );
                    assert!(template.is_default);
                    assert!(
                        template.rule.resources[0]
                            .attributes
                            .keys()
                            .all(|name| name == "workdir")
                    );
                    let mut rows = vec![None; offered.resources.len()];
                    rows[cargo_index] = Some(ComposedRow {
                        grant: PermissionRowGrant::Offered(template.id.clone()),
                        lifetime: PermissionLifetime::Project,
                    });
                    assert_eq!(
                        offered
                            .composed_rules(&rows)
                            .expect("composed template")
                            .len(),
                        1
                    );
                    PermissionAnswer::AllowComposed { rows }
                } else {
                    assert!(template.is_none());
                    PermissionAnswer::AllowOnce
                };
                assert!(manager.answer(&request.id, answer));
                enforcement.await.expect("approved call");
            }
            let saved = manager
                .structured_rule_inventory()
                .expect("saved inventory");
            assert_eq!(saved.len(), 1);
            assert_eq!(
                PermissionState::open(&state)
                    .expect("persisted state")
                    .records(),
                saved
            );
            let persisted = serde_json::to_string(&saved).expect("serialized inventory");
            assert!(!persisted.contains(COMMAND_OBSERVATION_ATTRIBUTE));
            assert!(
                saved[0].review.as_ref().expect("review").resources[0]
                    .value
                    .as_ref()
                    .expect("template label")
                    .contains("Command template")
            );
            drop(manager);
            let reloaded = PermissionManager::new_persistent_in(
                PermissionsConfig::default(),
                root.path().to_path_buf(),
                Arc::default(),
                state,
            );
            assert_eq!(
                reloaded
                    .structured_rule_inventory()
                    .expect("reloaded inventory"),
                saved
            );
            let mut input = input_for(PATTERN_PACKAGES[1]);
            input["timeoutSec"] = json!(PATTERN_TIMEOUT_SECS);
            input["workdir"] = json!(".");
            let approved = prepared_shell_request(root.path(), &registry, input, "reload").await;
            let rule = &saved[0].rule;
            assert!(permission_rule_covers_resource(
                rule,
                &approved,
                &approved.resources[cargo_index]
            ));
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let mut enforcement = Box::pin(enforce_shell_request(&reloaded, &approved, &events));
            assert!(
                future::poll_once(&mut enforcement)
                    .await
                    .expect("automatic decision")
                    .is_ok()
            );
            assert!(received.try_recv().is_err());
            let mut different_workdir = input_for(PATTERN_PACKAGES[1]);
            different_workdir["workdir"] = json!("crate");
            let different_workdir = prepared_shell_request(
                root.path(),
                &registry,
                different_workdir,
                "different-workdir",
            )
            .await;
            assert!(!permission_rule_covers_resource(
                rule,
                &different_workdir,
                &different_workdir.resources[cargo_index]
            ));
            let other = prepared_shell_request(
                root.path(),
                &registry,
                input_for(PATTERN_PACKAGES[0]),
                "other",
            )
            .await;
            for change in [
                "source",
                "input",
                "input_workdir",
                "timeout",
                "missing_binding",
                "fact_hash",
                "fact_context",
                "swapped_fact",
                "swapped_pair",
                "workdir",
                "possible_workdirs",
                "wire",
            ] {
                let mut changed = approved.clone();
                match change {
                    "source" => changed.resources[cargo_index].value.push_str(" --fix"),
                    "input" => changed.input["command"] = json!("cargo clean"),
                    "input_workdir" => changed.input["workdir"] = json!("crate"),
                    "timeout" => changed.input["timeoutSec"] = Value::Null,
                    "missing_binding" => {
                        changed.resources[cargo_index]
                            .attributes
                            .remove(COMMAND_OBSERVATION_BINDING_ATTRIBUTE);
                    }
                    "fact_hash" | "fact_context" => {
                        let mut fact = CommandObservation::from_json(
                            &changed.resources[cargo_index].attributes
                                [COMMAND_OBSERVATION_ATTRIBUTE],
                        )
                        .expect("native fact");
                        if change == "fact_hash" {
                            fact.source.input_hash = other.resources[cargo_index].attributes
                                [COMMAND_OBSERVATION_BINDING_ATTRIBUTE]
                                .clone();
                        } else {
                            fact.context.effective_workdir = "/elsewhere".into();
                        }
                        changed.resources[cargo_index].attributes.insert(
                            COMMAND_OBSERVATION_ATTRIBUTE.into(),
                            serde_json::to_string(&fact).expect("changed fact"),
                        );
                    }
                    "swapped_fact" | "swapped_pair" => {
                        changed.resources[cargo_index].attributes.insert(
                            COMMAND_OBSERVATION_ATTRIBUTE.into(),
                            other.resources[cargo_index].attributes[COMMAND_OBSERVATION_ATTRIBUTE]
                                .clone(),
                        );
                        if change == "swapped_pair" {
                            changed.resources[cargo_index].attributes.insert(
                                COMMAND_OBSERVATION_BINDING_ATTRIBUTE.into(),
                                other.resources[cargo_index].attributes
                                    [COMMAND_OBSERVATION_BINDING_ATTRIBUTE]
                                    .clone(),
                            );
                        }
                    }
                    "workdir" => {
                        changed.resources[cargo_index]
                            .attributes
                            .insert("workdir".into(), "/elsewhere".into());
                    }
                    "possible_workdirs" => {
                        changed.resources[cargo_index].attributes.insert(
                            POSSIBLE_WORKDIRS_ATTRIBUTE.into(),
                            json!({"kind":"known", "symbolic_paths":["/elsewhere"]}).to_string(),
                        );
                    }
                    "wire" => {
                        changed = serde_json::from_value(
                            serde_json::to_value(&changed).expect("wire request"),
                        )
                        .expect("restored request");
                    }
                    _ => unreachable!(),
                }
                assert!(
                    !permission_rule_covers_resource(
                        rule,
                        &changed,
                        &changed.resources[cargo_index]
                    ),
                    "{change}"
                );
            }
            let unobserved =
                prepared_shell_request(root.path(), &registry, input_for("delta"), "unobserved")
                    .await;
            assert!(!permission_rule_covers_resource(
                rule,
                &unobserved,
                &unobserved.resources[cargo_index]
            ));
        });
    }

    async fn prompted_shell_request(
        manager: &PermissionManager,
        request: &PermissionRequest,
        answer: impl FnOnce(&PermissionRequest) -> PermissionAnswer,
    ) -> PermissionRequest {
        let (events, received) = flume::unbounded();
        let events = EventSender::new(events, 0);
        let mut enforcement = Box::pin(enforce_shell_request(manager, request, &events));
        assert!(future::poll_once(&mut enforcement).await.is_none());
        let AgentEvent::PermissionRequest(offered) = received.try_recv().expect("prompt").event
        else {
            panic!("permission request")
        };
        assert!(manager.answer(&request.id, answer(&offered)));
        enforcement.await.expect("explicitly approved call");
        *offered
    }

    /// What the shell reads off a line decides whether Auto may ever screen
    /// it: privilege, indirection, and a line it cannot parse always ask, while
    /// a line it merely could not review waits for an engine.
    #[test_case("sudo ls", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "sudo")]
    #[test_case("doas ls", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "doas")]
    #[test_case("su -c ls", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "su")]
    #[test_case("eval x", AutoNote::AlwaysAsks(ShellOpacity::Indirect); "eval")]
    #[test_case("source env.sh", AutoNote::AlwaysAsks(ShellOpacity::Indirect); "source")]
    #[test_case(". ./env.sh", AutoNote::AlwaysAsks(ShellOpacity::Indirect); "dot")]
    #[test_case("bash -c 'sudo ls'", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "privilege_in_shell_code")]
    #[test_case("env sudo ls", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "privilege_behind_env")]
    #[test_case("xargs -n 1 sudo rm", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "privilege_behind_xargs")]
    #[test_case("for f in a; do sudo ls; done", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "privilege_in_a_loop")]
    #[test_case("echo $(sudo ls)", AutoNote::AlwaysAsks(ShellOpacity::Privilege); "privilege_in_a_substitution")]
    #[test_case("python3 - <<'PY'\nprint(1)\nPY\n", AutoNote::EngineNeeded; "a_heredoc_script_waits_for_an_engine")]
    #[test_case("for f in a b; do wc -l $f; done", AutoNote::EngineNeeded; "a_loop_waits_for_an_engine")]
    #[test_case("cargo check &&", AutoNote::AlwaysAsks(ShellOpacity::Unparsed); "a_syntax_error")]
    #[test_case("$EDITOR notes.md", AutoNote::AlwaysAsks(ShellOpacity::Unparsed); "a_dynamic_executable")]
    #[test_case("git status > status.txt", AutoNote::EngineNeeded; "a_screenable_line_waits_for_an_engine")]
    fn auto_always_asks_for_privilege_indirect_and_unparsed(command: &str, note: AutoNote) {
        smol::block_on(async {
            let root = TempDir::new().expect("project");
            let (_host, registry) = host_and_registry(root.path());
            let manager = PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    decision_engine: true,
                    ..PermissionsConfig::default()
                },
                root.path().to_path_buf(),
                Arc::default(),
            );
            manager.set_session_mode(Some(PermissionMode::Auto));
            let request = prepared_shell_request(
                root.path(),
                &registry,
                json!({"command": command}),
                command,
            )
            .await;
            let offered =
                prompted_shell_request(&manager, &request, |_| PermissionAnswer::AllowOnce).await;
            assert_eq!(offered.presentation.auto, Some(note));
        });
    }

    #[test]
    fn generic_shell_templates_require_consent_and_respect_explicit_regex_domains() {
        smol::block_on(async {
            let root = TempDir::new().expect("project");
            let (_host, registry) = host_and_registry(root.path());
            let manager = PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                root.path().to_path_buf(),
                Arc::default(),
            );
            let input_for =
                |name: &str| json!({"command": format!("novelctl inspect --name {name}")});
            for (index, name) in PATTERN_PACKAGES.iter().enumerate() {
                let request =
                    prepared_shell_request(root.path(), &registry, input_for(name), name).await;
                let resource = &request.resources[0];
                assert!(!resource.attributes.contains_key(CONFINED_READ_ATTRIBUTE));
                let fact = CommandObservation::from_json(
                    &resource.attributes[COMMAND_OBSERVATION_ATTRIBUTE],
                )
                .expect("generic execution observation");
                assert_eq!(
                    fact.roles,
                    [
                        ArgumentRole::Executable,
                        ArgumentRole::Operation,
                        ArgumentRole::Flag,
                        ArgumentRole::Unknown
                    ]
                );
                let offered =
                    prompted_shell_request(&manager, &request, |_| PermissionAnswer::AllowOnce)
                        .await;
                let template = offered
                    .options
                    .iter()
                    .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX));
                assert_eq!(template.is_some(), index + 1 == PATTERN_PACKAGES.len());
                if let Some(template) = template {
                    assert!(template.is_default);
                    assert!(!template.label.contains(COMMAND_TEMPLATE_EXECUTION_NOTICE));
                    assert!(
                        template
                            .description
                            .contains(COMMAND_TEMPLATE_EXECUTION_NOTICE)
                    );
                    let PermissionResourceSelector::CommandTemplate { definition } =
                        &template.rule.resources[0].selector
                    else {
                        panic!("command template")
                    };
                    assert!(matches!(
                        &definition.argv[3],
                        PatternToken::Slot {
                            role: ArgumentRole::Unknown,
                            ..
                        }
                    ));
                    assert!(
                        matches!(&definition.combinations, SlotCombinations::ObservedTuples { tuples } if tuples.len() == PATTERN_PACKAGES.len())
                    );
                    assert_eq!(definition.slots[0].option_like, OptionLikePolicy::Reject);
                    assert!(
                        matches!(&definition.slots[0].domain, ArgumentDomain::ObservedSet { values }
                        if values.iter().map(String::as_str).collect::<Vec<_>>() == PATTERN_PACKAGES)
                    );
                    let review = review_for_rule(&offered, &template.rule);
                    assert!(
                        review.resources[0]
                            .value
                            .as_ref()
                            .expect("execution review")
                            .contains(COMMAND_TEMPLATE_EXECUTION_NOTICE)
                    );
                }
                assert!(
                    manager
                        .structured_rule_inventory()
                        .expect("inventory")
                        .is_empty()
                );
            }
            let unseen = prepared_shell_request(
                root.path(),
                &registry,
                input_for("delta-42"),
                "regex-value",
            )
            .await;
            let request = prepared_shell_request(
                root.path(),
                &registry,
                input_for(PATTERN_PACKAGES[0]),
                "consent",
            )
            .await;
            prompted_shell_request(&manager, &request, |offered| {
                let template = offered
                    .options
                    .iter()
                    .find(|option| option.id.starts_with(COMMAND_TEMPLATE_PREFIX))
                    .expect("still requires template consent");
                assert!(!permission_rule_covers_request(&template.rule, &unseen));
                let PermissionResourceSelector::CommandTemplate { definition } =
                    &template.rule.resources[0].selector
                else {
                    panic!("command template")
                };
                let mut edited = definition.clone();
                edited.slots[0].domain = ArgumentDomain::Regex {
                    pattern: GENERIC_NAME_REGEX.into(),
                };
                let row = |definition| {
                    vec![Some(ComposedRow {
                        grant: PermissionRowGrant::Pattern {
                            option_id: template.id.clone(),
                            definition,
                        },
                        lifetime: PermissionLifetime::Conversation,
                    })]
                };
                let tuple_rules = offered
                    .composed_rules(&row(edited.clone()))
                    .expect("tuple-bound regex");
                assert!(!permission_rule_covers_request(&tuple_rules[0], &unseen));
                edited.combinations = SlotCombinations::Independent;
                let rows = row(edited);
                let rules = offered
                    .composed_rules(&rows)
                    .expect("explicit regex domain");
                assert!(permission_rule_covers_request(&rules[0], &unseen));
                let review = review_for_rule(offered, &rules[0]);
                let label = review.resources[0]
                    .value
                    .as_ref()
                    .expect("edited template review");
                assert!(label.contains(COMMAND_TEMPLATE_EXECUTION_NOTICE));
                assert!(label.contains(GENERIC_NAME_REGEX));
                PermissionAnswer::AllowComposed { rows }
            })
            .await;
            let saved = manager.structured_rule_inventory().expect("explicit grant");
            assert_eq!(saved.len(), 1);
            assert!(permission_rule_covers_request(&saved[0].rule, &unseen));
            let (events, received) = flume::unbounded();
            let events = EventSender::new(events, 0);
            let mut enforcement = Box::pin(enforce_shell_request(&manager, &unseen, &events));
            assert!(
                future::poll_once(&mut enforcement)
                    .await
                    .expect("configured regex match")
                    .is_ok()
            );
            assert!(received.try_recv().is_err());
            for (index, command) in [
                "novelctl inspect --name delta-nope",
                "novelctl inspect --name delta-42 --force",
                "novelctl remove --name delta-42",
                "novelctl inspect nested --name delta-42",
                "novelctl inspect --other delta-42",
                "differentctl inspect --name delta-42",
            ]
            .iter()
            .enumerate()
            {
                let changed = prepared_shell_request(
                    root.path(),
                    &registry,
                    json!({"command": command}),
                    &format!("mismatch-{index}"),
                )
                .await;
                assert!(
                    changed.resources[0]
                        .attributes
                        .contains_key(COMMAND_OBSERVATION_ATTRIBUTE)
                );
                assert!(
                    !permission_rule_covers_request(&saved[0].rule, &changed),
                    "{command}"
                );
                prompted_shell_request(&manager, &changed, |_| PermissionAnswer::AllowOnce).await;
                assert_eq!(
                    manager
                        .structured_rule_inventory()
                        .expect("unchanged grant"),
                    saved
                );
            }
        });
    }

    fn shell_output(exit_code: i32) -> WorkcellShellOutput {
        WorkcellShellOutput {
            version: 1,
            kind: "shell",
            relative_workdir: ".".into(),
            timeout_ms: 120_000,
            duration_ms: 10,
            exit_code: Some(exit_code),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 1,
            stdout_utf8_bytes: 3,
            stderr_utf8_bytes: 0,
            stdout: "raw".into(),
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 190,
            stderr_redraws_collapsed: 2,
        }
    }

    #[test]
    fn shell_result_keeps_raw_output_and_surfaces_exit_status_to_the_model() {
        let success = shell_result_parts(
            shell_output(0),
            "filtered\n[filtered: make, progress]".into(),
            Some(WorkcellShellFilterInfo {
                stages: vec!["make".into(), "progress".into()],
                unfiltered_utf8_bytes: 100,
                filtered_utf8_bytes: 20,
            }),
        );
        assert!(!success.is_error);
        assert_eq!(
            success.model_output.as_deref(),
            Some("filtered\n[filtered: make, progress]\n\n[shell status: exit code 0]")
        );
        let ToolOutput::Shell(output) = success.output.unwrap() else {
            panic!("expected typed shell output");
        };
        assert_eq!(output.stdout, "raw");
        assert_eq!(
            output.model_text,
            "filtered\n[filtered: make, progress]\n\n[shell status: exit code 0]"
        );
        // Every reduction is named, and rendering is disclosed separately
        // because it can absorb frames with no rule matching at all.
        assert_eq!(output.redraws_collapsed(), 192);
        assert_eq!(output.filter.unwrap().stages, ["make", "progress"]);

        let failure = shell_result_parts(shell_output(101), "filtered".into(), None);
        assert!(failure.is_error);
        assert_eq!(
            failure.model_output.as_deref(),
            Some("filtered\n\n[shell status: exit code 101]")
        );
        assert!(matches!(
            failure.output.as_ref().unwrap(),
            ToolOutput::Shell(output) if output.filter.is_none()
        ));
    }

    /// Workcell's flags place a command's failure, never its output. A host
    /// returns a finished command as a success whatever its exit, so the remote
    /// result keeps the command's own status over the envelope's.
    #[test_case(0, false, false => (false, None) ; "a_clean_exit")]
    #[test_case(FAILING_EXIT_CODE, false, false => (true, None) ; "a_failed_command_states_no_reason")]
    #[test_case(0, false, true => (true, None) ; "an_exceeded_output_limit_states_no_reason")]
    #[test_case(FAILING_EXIT_CODE, true, false => (true, Some(ToolFailure::Timeout)) ; "a_timed_out_command")]
    fn a_command_fails_by_its_flags_locally_and_remotely(
        exit_code: i32,
        timed_out: bool,
        output_limit_exceeded: bool,
    ) -> (bool, Option<ToolFailure>) {
        let mut output = shell_output(exit_code);
        output.stdout = MISLEADING_OUTPUT.into();
        output.timed_out = timed_out;
        output.output_limit_exceeded = output_limit_exceeded;
        let remote = remote_result(
            ToolKind::Shell,
            &Input::parse(ToolKind::Shell, json!({"command": DEADLINE_COMMAND}))
                .expect("valid input"),
            RemoteToolResultEnvelope {
                structured_content: serde_json::to_value(&output).expect("structured output"),
                model_output: MISLEADING_OUTPUT.into(),
                is_error: false,
            },
        );
        let local = shell_result_parts(output, MISLEADING_OUTPUT.into(), None);

        assert_eq!(
            (remote.is_error, remote.failure),
            (local.is_error, local.failure)
        );
        (local.is_error, local.failure)
    }

    fn code_output(outcome: Outcome, timed_out: bool) -> CodeOutput {
        CodeOutput {
            version: 1,
            kind: "code",
            outcome,
            timeout_ms: CODE_DEFAULT_TIMEOUT_MS,
            duration_ms: 10,
            type_checked: true,
            result: Value::Null,
            result_repr: None,
            stdout: MISLEADING_OUTPUT.into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_utf8_bytes: 0,
            stderr_utf8_bytes: 0,
            exception: None,
            diagnostic: None,
            timed_out,
            memory_exceeded: false,
            suspension_limit_exceeded: false,
        }
    }

    /// The outcome Workcell typed places a snippet's failure, and reads the
    /// same whether the worker ran here or on a remote host.
    #[test_case(Outcome::Completed, false => (false, None) ; "completed")]
    #[test_case(Outcome::Rejected, false => (true, Some(ToolFailure::InvalidInput)) ; "rejected_before_running")]
    #[test_case(Outcome::Exception, false => (true, Some(ToolFailure::Other)) ; "raised")]
    #[test_case(Outcome::Limited, true => (true, Some(ToolFailure::Timeout)) ; "out_of_time")]
    #[test_case(Outcome::Limited, false => (true, Some(ToolFailure::Other)) ; "out_of_another_budget")]
    #[test_case(Outcome::Unavailable, false => (true, Some(ToolFailure::Other)) ; "unavailable")]
    fn a_snippet_fails_by_its_outcome_locally_and_remotely(
        outcome: Outcome,
        timed_out: bool,
    ) -> (bool, Option<ToolFailure>) {
        let remote = remote_result(
            ToolKind::Code,
            &Input::parse(ToolKind::Code, json!({"code": DEADLINE_CODE})).expect("valid input"),
            RemoteToolResultEnvelope {
                structured_content: serde_json::to_value(code_output(outcome, timed_out))
                    .expect("structured output"),
                model_output: MISLEADING_OUTPUT.into(),
                is_error: false,
            },
        );
        let local = code_result(CodeExecution {
            output: code_output(outcome, timed_out),
            model_text: MISLEADING_OUTPUT.into(),
        });

        assert_eq!(
            (remote.is_error, remote.failure),
            (local.is_error, local.failure)
        );
        (local.is_error, local.failure)
    }

    #[test]
    fn no_rtk_disables_native_shell_output_filtering() {
        if std::process::Command::new("make")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = TempDir::new().expect("tempdir");
        std::fs::write(root.path().join("Makefile"), FILTERABLE_MAKEFILE).unwrap();
        let (_host, registry) = host_and_registry(root.path());

        for (no_rtk, filtered) in [(false, true), (true, false)] {
            let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
            ctx.config = caudra_config::RawConfig::default()
                .into_config(no_rtk)
                .unwrap()
                .agent;
            let entry = registry.get("shell").expect("registered shell");
            let invocation = entry
                .tool
                .parse(&json!({"command": "make all"}))
                .expect("valid shell input");
            smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");
            let result = smol::block_on(invocation.execute(&ctx));
            let ToolOutput::Shell(output) = result.output.unwrap() else {
                panic!("expected typed shell output");
            };

            assert_eq!(output.filter.is_some(), filtered);
            assert_eq!(output.model_text.contains("Entering directory"), !filtered);
            assert!(output.stdout.contains("Entering directory"));
        }
    }

    /// A tree with a clear gradient: `normalize_sku` is called from three
    /// places and `orphan_helper` from none.
    fn code_graph_tree(root: &Path) {
        for (path, source) in [
            (
                "src/normalize.rs",
                "pub fn normalize_sku(input: &str) -> String { input.trim().to_owned() }\n",
            ),
            (
                "src/catalog.rs",
                "use crate::normalize::normalize_sku;\npub fn add_item(sku: &str) { normalize_sku(sku); }\npub fn update_item(sku: &str) { normalize_sku(sku); }\n",
            ),
            (
                "src/import.rs",
                "use crate::normalize::normalize_sku;\npub fn import_row(sku: &str) { normalize_sku(sku); }\npub fn orphan_helper() {}\n",
            ),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, source).expect("write");
        }
    }

    fn run_code_graph(root: &Path, tool: &str, input: Value) -> ToolExecResult {
        let (_host, registry) = host_and_registry(root);
        let ctx = context(root, Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get(tool)
            .unwrap_or_else(|| panic!("registered {tool}"))
            .tool
            .parse(&input)
            .expect("valid code-graph input");
        smol::block_on(invocation.preflight(&ctx)).expect("code-graph preflight");
        smol::block_on(invocation.execute(&ctx))
    }

    #[test_case("code_map", "code.map.v1", "search" ; "map")]
    #[test_case("code_context", "code.context.v1", "search" ; "context")]
    #[test_case("code_refs", "code.refs.v1", "search" ; "refs")]
    #[test_case("code_impact", "code.impact.v1", "search" ; "impact")]
    #[test_case("code_expand", "code.expand.v1", "read" ; "expand")]
    fn code_graph_tools_register_read_only_with_workcell_contracts(
        tool: &str,
        contract: &str,
        kind: &str,
    ) {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let registered = registry.get(tool).expect("registered code-graph tool");

        assert_eq!(registered.tool.tool_kind(), Some(kind));
        assert_eq!(registered.effect, ToolEffect::ReadOnly);
        assert!(matches!(
            registered.source,
            ToolSource::Native {
                ref owner,
                contract: ref registered,
                trusted: true,
            } if owner.as_ref() == OWNER && registered.as_ref() == contract
        ));
    }

    #[test]
    fn code_map_ranks_the_referenced_symbol_above_the_orphan() {
        let root = TempDir::new().expect("tempdir");
        code_graph_tree(root.path());
        let result = run_code_graph(root.path(), "code_map", json!({}));

        let Ok(ToolOutput::CodeGraph { rows, footer, .. }) = result.output else {
            panic!("expected a code-graph card");
        };
        let ranked: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            ranked.first(),
            Some(&"normalize_sku"),
            "three callers must outrank an uncalled helper, got {ranked:?}"
        );
        assert!(
            rows.iter()
                .find(|row| row.name == "normalize_sku")
                .is_some_and(|row| row.inbound == Some(3)),
            "the row must carry the reference count it was ranked by"
        );
        assert!(footer.contains("symbols"), "footer was {footer:?}");
    }

    /// The card must never restate Workcell's rendering: the floor caveats and
    /// the truncation notice live in that text, and a second copy here would be
    /// a second contract to keep in step.
    #[test]
    fn the_model_sees_workcells_own_rendering() {
        const LEGEND: &str = "counts are floors";

        let root = TempDir::new().expect("tempdir");
        code_graph_tree(root.path());
        let result = run_code_graph(root.path(), "code_map", json!({}));
        let model_output = result.model_output.expect("model output");

        assert!(model_output.contains(LEGEND), "got {model_output:?}");
        assert!(model_output.contains("normalize_sku"));
    }

    #[test]
    fn code_impact_rows_carry_hop_distance_rather_than_reference_counts() {
        let root = TempDir::new().expect("tempdir");
        code_graph_tree(root.path());
        let result = run_code_graph(
            root.path(),
            "code_impact",
            json!({ "symbol": "normalize_sku" }),
        );

        let Ok(ToolOutput::CodeGraph { rows, .. }) = result.output else {
            panic!("expected a code-graph card");
        };
        assert!(!rows.is_empty(), "three callers reach this symbol");
        assert!(
            rows.iter()
                .all(|row| row.hops.is_some() && row.inbound.is_none()),
            "a reach row measures distance, never references"
        );
    }

    /// A well-formed question about a symbol that does not exist is answered
    /// with candidates, not failed: an error envelope has nowhere to put them.
    #[test]
    fn an_unknown_symbol_is_refused_as_a_successful_call() {
        let root = TempDir::new().expect("tempdir");
        code_graph_tree(root.path());
        let result = run_code_graph(
            root.path(),
            "code_refs",
            json!({ "symbol": "normalise_sku" }),
        );

        assert!(
            result.output.is_ok(),
            "a refusal is a result, not a tool error"
        );
        let model_output = result.model_output.expect("model output");
        assert!(
            model_output.contains("normalize_sku"),
            "the did-you-mean list is the useful answer, got {model_output:?}"
        );
    }

    /// A crawl reads every source file under the scope, so the grant is the
    /// directory. A narrower scope would claim an access the tool does not have.
    #[test]
    fn a_code_graph_call_asks_for_the_subtree_it_crawls() {
        let root = TempDir::new().expect("tempdir");
        code_graph_tree(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("code_map")
            .expect("registered code_map")
            .tool
            .parse(&json!({ "path": "src" }))
            .expect("valid input");

        let intent = smol::block_on(invocation.preflight(&ctx))
            .expect("preflight")
            .expect("an intent");
        assert_eq!(intent.risk, PermissionRisk::Low);
        assert!(
            intent
                .scopes
                .scopes
                .iter()
                .all(|scope| scope.ends_with("/**")),
            "a crawl grants a subtree, got {:?}",
            intent.scopes.scopes
        );
        assert!(invocation.mutation_targets(&ctx).is_empty());
    }

    #[test]
    fn terminal_rendering_survives_no_rtk_because_it_is_decoding() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        ctx.config = caudra_config::RawConfig::default()
            .into_config(true)
            .unwrap()
            .agent;
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": r"printf '1/3\r2/3\r3/3\n'"}))
            .expect("valid shell input");
        smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");
        let ToolOutput::Shell(output) = smol::block_on(invocation.execute(&ctx)).output.unwrap()
        else {
            panic!("expected typed shell output");
        };

        // Disabling the filter withholds a judgement about content. It does not
        // ask for a control stream back, so the capture is still the row a
        // terminal would have shown.
        assert!(output.filter.is_none());
        assert_eq!(output.stdout, "3/3\n");
        assert_eq!(output.redraws_collapsed(), 2);
    }

    fn pane_canary(name: &str) -> String {
        format!("{name}{PANE_CANARY_SUFFIX}")
    }

    /// The `herdr` skill stops unless a command sees `HERDR_ENV`, and it
    /// addresses the pane through the other variables. Setting them in this
    /// process takes `unsafe`, so a copy of this binary runs the command.
    #[test]
    fn shell_commands_see_the_herdr_pane_they_run_in() {
        let child = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", HERDR_PANE_CHILD, "--nocapture"])
            .env(CHILD_TEST_ENV, HERDR_PANE_CHILD)
            .envs(
                PANE_ENVIRONMENT
                    .iter()
                    .map(|&name| (name, pane_canary(name))),
            )
            .output()
            .expect("child test process");
        let stdout = String::from_utf8_lossy(&child.stdout);

        assert!(
            child.status.success() && stdout.contains(HERDR_PANE_CHILD_PASSED),
            "{stdout}{}",
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[test]
    fn herdr_pane_environment_child() {
        if std::env::var(CHILD_TEST_ENV).as_deref() != Ok(HERDR_PANE_CHILD) {
            return;
        }
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": format!("printenv {}", PANE_ENVIRONMENT.join(" "))}))
            .expect("valid shell input");
        smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");
        let ToolOutput::Shell(output) = smol::block_on(invocation.execute(&ctx)).output.unwrap()
        else {
            panic!("expected typed shell output");
        };

        let expected: Vec<String> = PANE_ENVIRONMENT
            .iter()
            .map(|&name| pane_canary(name))
            .collect();
        assert_eq!(output.stdout.lines().collect::<Vec<_>>(), expected);
        println!("{HERDR_PANE_CHILD_PASSED}");
    }

    fn progress_chunk(stream: ShellStream, text: &str) -> ShellProgressChunk {
        ShellProgressChunk {
            version: 1,
            sequence: 0,
            stream,
            text: text.into(),
        }
    }

    #[test]
    fn a_live_bar_is_one_updating_row_that_does_not_evict_what_preceded_it() {
        let mut tail = ProgressTail::default();
        tail.push(&progress_chunk(ShellStream::Stdout, "loading dataset\n"));
        let mut shown = String::new();
        for step in 0..400 {
            shown = tail.push(&progress_chunk(
                ShellStream::Stdout,
                &format!("\r{step:>3}/400 [{}]", "#".repeat(step / 20)),
            ));
        }

        // Splitting the raw stream on newlines would make all 400 frames one
        // row several kilobytes wide, and long enough runs would push the line
        // printed before the bar out of the retained window entirely.
        assert_eq!(shown.lines().count(), 2, "{shown}");
        assert_eq!(shown.lines().next(), Some("loading dataset"));
        assert_eq!(
            shown.lines().next_back(),
            Some("399/400 [###################]")
        );
    }

    #[test]
    fn a_running_command_publishes_rendered_rows_rather_than_a_control_stream() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let (tx, rx) = flume::unbounded::<Envelope>();
        let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        ctx.event_tx = EventSender::new(tx, 0);
        ctx.tool_use_id = Some("shell-call".into());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": r"printf 'setup done\n0%%\r50%%\r100%%\n'"}))
            .expect("valid shell input");

        smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");
        smol::block_on(invocation.execute(&ctx));

        let published: Vec<String> = rx
            .drain()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::ToolOutput { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        let last = published
            .last()
            .expect("a running command publishes output");
        assert_eq!(last.lines().collect::<Vec<_>>(), ["setup done", "100%"]);
    }

    fn observed_rows(live: &ShellLive) -> Vec<String> {
        live.lines()
            .expect("the executor attaches its buffer")
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn a_tracked_local_shell_publishes_rows_without_a_transcript_sink() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let live = ShellLive::default();
        ctx.shell_live = Some(live.clone());
        let invocation = registry
            .get("shell")
            .expect("registered shell")
            .tool
            .parse(&json!({"command": format!("printf '{OBSERVED_ROW}\\n'")}))
            .expect("valid shell input");

        smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");
        smol::block_on(invocation.execute(&ctx));

        assert!(ctx.live_sink.is_none());
        assert_eq!(observed_rows(&live), [OBSERVED_ROW]);
    }

    #[test]
    fn a_tracked_remote_shell_publishes_rows_without_a_transcript_sink() {
        let root = TempDir::new().unwrap();
        let mut ctx = context(
            root.path(),
            Arc::new(ToolRegistry::new()),
            CancelToken::none(),
        );
        let live = ShellLive::default();
        ctx.shell_live = Some(live.clone());
        let execution_id = OperationId::new("execution").unwrap();
        let status = OperationStatus {
            handle: OperationHandle {
                preparation_id: OperationId::new("preparation").unwrap(),
                invocation_id: Some(OperationId::new("invocation").unwrap()),
                execution_id: Some(execution_id.clone()),
                expires_at_unix_ms: Some(1),
            },
            state: OperationState::Running,
            progress: vec![OperationProgress {
                execution_id,
                sequence: 1,
                kind: OperationProgressKind::Stdout,
                chunk: OBSERVED_ROW.into(),
            }],
            progress_metadata: SequenceMetadata {
                first_retained_sequence: Some(1),
                next_sequence: 2,
                gap_before_first: false,
            },
        };

        assert!(RemoteProgress::new(&ctx).publish(&status));
        assert_eq!(observed_rows(&live), [OBSERVED_ROW]);
    }

    #[test]
    fn a_bar_on_one_stream_does_not_overwrite_the_other() {
        // A row belongs to the stream that drew it, so the renderers are
        // separate; the buffer is shared because arrival order is what a reader
        // saw.
        let mut tail = ProgressTail::default();
        tail.push(&progress_chunk(ShellStream::Stdout, "compiling\n"));
        let shown = tail.push(&progress_chunk(ShellStream::Stderr, "  0%\r 50%"));
        assert_eq!(shown, "compiling\n 50%");
    }

    #[test_case(workcell::code_graph::GraphPhase::Crawl, 0, "crawl" ; "crawl_started")]
    #[test_case(workcell::code_graph::GraphPhase::Crawl, 64, "crawl 64 files" ; "crawl_progress")]
    #[test_case(workcell::code_graph::GraphPhase::Parse, 12, "parse 12 files" ; "parse_progress")]
    #[test_case(workcell::code_graph::GraphPhase::Rank, 12, "rank 12 files" ; "rank_progress")]
    fn code_graph_progress_is_exposed_as_a_live_annotation(
        phase: workcell::code_graph::GraphPhase,
        files: usize,
        expected: &str,
    ) {
        let (live_sink, live) = flume::bounded(1);
        let sink = GraphPhaseSink { live_sink };

        smol::block_on(sink.publish(GraphProgress { phase, files }));

        let ToolLive::Annotation(annotation) = live.recv().expect("one progress annotation") else {
            panic!("graph progress must use the annotation surface");
        };
        assert_eq!(annotation, expected);
    }

    /// Exercised through `from_values` rather than the process environment:
    /// env vars are global to the test binary and would race every other test.
    #[test_case(None, None, None ; "no_proxy_configured")]
    #[test_case(Some("http://proxy.internal:8080"), None, None ; "http_only")]
    #[test_case(None, Some("http://proxy.internal:8080"), None ; "https_only")]
    #[test_case(None, None, Some("http://proxy.internal:3128") ; "all_supplies_both")]
    fn a_usable_proxy_environment_is_accepted(
        http: Option<&str>,
        https: Option<&str>,
        all: Option<&str>,
    ) {
        let configured = http.is_some() || https.is_some() || all.is_some();
        let proxy = ProxyConfiguration::from_values(http, https, all, None).expect("usable");
        assert_eq!(proxy.is_direct(), !configured);
    }

    #[test_case("socks5://proxy.internal:1080" ; "unsupported_scheme")]
    #[test_case("not a url" ; "unparseable")]
    fn an_unusable_proxy_value_degrades_to_a_direct_dial(value: &str) {
        const DIRECT_NOTICE: &str = "dialling directly";

        let error = ProxyConfiguration::from_values(None, None, Some(value), None)
            .expect_err("must not be accepted");
        let warning = format!(
            "Workcell web tools are dialling directly: the proxy environment is unusable ({error})"
        );
        assert!(warning.contains(DIRECT_NOTICE));
        assert!(
            !warning.contains("proxy.internal"),
            "a proxy URL can carry credentials and must never reach a log line"
        );
    }

    /// `caudra-config` keeps its own copy of this list to answer `--help` and
    /// the disable flags without depending on this crate. Nothing else forces
    /// the two to agree, and a name that drifts out of the config copy silently
    /// stops being a recognized built-in.
    #[test]
    fn the_config_copy_of_the_tool_roster_matches_this_one() {
        let mut ours: Vec<&str> = NATIVE_TOOL_NAMES.to_vec();
        let mut theirs: Vec<&str> = caudra_config::WORKCELL_NATIVE_TOOL_NAMES.to_vec();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours, theirs);
    }

    /// Every spec Workcell publishes has to resolve to a `ToolKind`. `entries`
    /// drops the ones that do not, so an upstream rename would otherwise remove
    /// a tool from the registry without a word.
    #[test]
    fn every_published_spec_resolves_to_a_tool_kind() {
        let mut published: Vec<&str> = workcell::files::specs(ALLOW_WRITE)
            .into_iter()
            .chain(workcell::web::specs(
                2026,
                &WebsearchExecutionConfiguration::default(),
            ))
            .chain(workcell::shell::specs())
            .chain(workcell::code::specs())
            .chain(workcell::code_graph::specs())
            .chain(std::iter::once(workcell::environment::spec()))
            .map(|spec| spec.name)
            .collect();
        published.sort_unstable();
        let mut known: Vec<&str> = NATIVE_TOOL_NAMES.to_vec();
        known.sort_unstable();
        assert_eq!(published, known);
    }

    #[test]
    fn documented_catalog_preserves_canonical_order_contracts_and_effects() {
        let root = TempDir::new().expect("tempdir");
        let host = WorkcellHost::new(root.path(), None).expect("Workcell host");
        let registry = ToolRegistry::new();
        host.register_documented_tools(&registry)
            .expect("Workcell registration");
        let expected = [
            ("file_read", "file.read.v1", ToolEffect::ReadOnly, "read"),
            ("file_glob", "file.glob.v1", ToolEffect::ReadOnly, "search"),
            ("file_grep", "file.grep.v1", ToolEffect::ReadOnly, "search"),
            ("file_write", "file.write.v1", ToolEffect::Mutating, "edit"),
            ("file_edit", "file.edit.v1", ToolEffect::Mutating, "edit"),
            (
                "file_apply_patch",
                "file.patch.v1",
                ToolEffect::Mutating,
                "edit",
            ),
            ("file_index", "file.index.v1", ToolEffect::ReadOnly, "read"),
            ("websearch", "web.search.v1", ToolEffect::ReadOnly, "search"),
            ("webfetch", "web.fetch.v1", ToolEffect::ReadOnly, "fetch"),
            (
                "shell",
                "shell.execution.v1",
                ToolEffect::Mutating,
                "execute",
            ),
            ("code_map", "code.map.v1", ToolEffect::ReadOnly, "search"),
            (
                "code_context",
                "code.context.v1",
                ToolEffect::ReadOnly,
                "search",
            ),
            ("code_refs", "code.refs.v1", ToolEffect::ReadOnly, "search"),
            (
                "code_impact",
                "code.impact.v1",
                ToolEffect::ReadOnly,
                "search",
            ),
            (
                "code_expand",
                "code.expand.v1",
                ToolEffect::ReadOnly,
                "read",
            ),
            (
                "python_execution",
                "python.execution.v1",
                ToolEffect::Isolated,
                "execute",
            ),
            (
                "execution_environment",
                "execution-environment.snapshot.v1",
                ToolEffect::Mutating,
                "execute",
            ),
        ];
        let snapshot = registry.iter();
        let names: Vec<&str> = snapshot.iter().map(|entry| entry.name()).collect();
        let expected_names: Vec<&str> = expected.iter().map(|entry| entry.0).collect();

        assert_eq!(names, expected_names);
        for (registered, (name, contract, effect, presentation)) in snapshot.iter().zip(expected) {
            assert_eq!(registered.name(), name);
            assert_eq!(registered.effect, effect, "{name}");
            assert_eq!(registered.tool.tool_kind(), Some(presentation), "{name}");
            assert!(matches!(
                &registered.source,
                ToolSource::Native {
                    owner,
                    contract: registered_contract,
                    trusted: true,
                } if owner.as_ref() == OWNER && registered_contract.as_ref() == contract
            ));
        }
    }

    #[test]
    fn documented_catalog_input_schema_contracts_are_stable() {
        let root = TempDir::new().expect("tempdir");
        let host = WorkcellHost::new(root.path(), None).expect("Workcell host");
        let registry = ToolRegistry::new();
        host.register_documented_tools(&registry)
            .expect("Workcell registration");

        for (name, expected) in canonical_input_schemas() {
            let registered = registry
                .get(name)
                .unwrap_or_else(|| panic!("registered {name}"));
            let actual = schema_contract(&registered.tool.schema());
            assert_eq!(actual, expected, "input schema contract changed for {name}");
        }
    }

    #[test]
    fn missing_worker_omits_code_without_affecting_other_tools() {
        let root = TempDir::new().expect("tempdir");
        let (host, registry) = host_and_registry(root.path());

        assert!(registry.get("python_execution").is_none());
        assert!(registry.get("file_read").is_some());
        assert!(registry.get("shell").is_some());
        assert!(registry.get("execution_environment").is_some());
        assert_eq!(registry.iter().len(), NATIVE_TOOL_NAMES.len() - 1);
        assert!(
            host.warnings()
                .iter()
                .any(|warning| warning.contains("python_execution"))
        );
    }

    #[test]
    fn production_host_executes_code_with_the_embedded_worker() {
        if !bundled_worker_available() {
            assert!(
                option_env!("WORKCELL_BUNDLED_MONTY_WORKER").is_none(),
                "the configured worker was not embedded"
            );
            return;
        }
        let root = TempDir::new().expect("tempdir");
        let host = WorkcellHost::new_production(root.path(), None).expect("Workcell host");
        assert!(host.warnings().is_empty());
        let registry = Arc::new(ToolRegistry::new());
        host.register(&registry).expect("Workcell registration");
        drop(host);
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("python_execution")
            .expect("registered code execution")
            .tool
            .parse(&json!({"code": "sum([1, 2, 3, 4])"}))
            .expect("valid code input");

        smol::block_on(invocation.preflight(&ctx)).expect("code preflight");
        let result = smol::block_on(invocation.execute(&ctx));

        assert!(!result.is_error);
        assert_eq!(result.model_output.as_deref(), Some("result: 10"));
        let output = result.output.expect("successful tool output");
        let state = output.state().expect("structured code output");
        assert_eq!(state["outcome"], "completed");
        assert_eq!(state["result"], 10);
    }

    #[test]
    fn registered_schema_is_the_workcell_schema_and_parsing_is_strict() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let expected = workcell::files::specs(ALLOW_WRITE)
            .into_iter()
            .find(|spec| spec.name == "file_read")
            .expect("file_read spec");
        let registered = registry.get("file_read").expect("registered file_read");

        assert_eq!(
            registered.tool.schema(),
            Value::Object(expected.input_schema)
        );
        assert!(
            registered
                .tool
                .parse(&json!({"filePath": "a.txt", "unknown": true}))
                .is_err()
        );
    }

    #[test]
    fn index_uses_workcell_schema_contract_and_native_policy() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let expected = workcell::files::specs(ALLOW_WRITE)
            .into_iter()
            .find(|spec| spec.name == "file_index")
            .expect("index spec");
        let registered = registry.get("file_index").expect("registered index");

        assert_eq!(
            registered.tool.schema(),
            Value::Object(expected.input_schema)
        );
        assert_eq!(registered.tool.audience(), ToolAudience::all());
        assert_eq!(registered.tool.tool_kind(), Some("read"));
        assert_eq!(registered.effect, ToolEffect::ReadOnly);
        assert!(matches!(
            registered.source,
            ToolSource::Native {
                ref owner,
                ref contract,
                trusted: true,
            } if owner.as_ref() == OWNER && contract.as_ref() == "file.index.v1"
        ));
        assert!(
            registered
                .tool
                .parse(&json!({"path": "src/lib.rs"}))
                .is_ok()
        );
        assert!(registered.tool.parse(&json!({})).is_err());
        assert!(
            registered
                .tool
                .parse(&json!({"path": "src/lib.rs", "maxSourceBytes": 1}))
                .is_err()
        );

        for audience in [
            ToolAudience::MAIN,
            ToolAudience::RESEARCH_SUB,
            ToolAudience::GENERAL_SUB,
            ToolAudience::INTERPRETER,
        ] {
            let definitions = registry.definitions(
                &caudra_agent::template::Vars::new(),
                &DescriptionContext {
                    filter: &caudra_agent::tools::ToolFilter::All,
                    audience,
                    workflows_available: false,
                },
                false,
            );
            assert!(
                definitions
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|definition| definition["name"] == "file_index"),
                "index missing for {audience:?}"
            );
        }
    }

    #[test]
    fn registering_workcell_twice_does_not_duplicate_index() {
        let root = TempDir::new().expect("tempdir");
        let host = WorkcellHost::new(root.path(), None).unwrap();
        let registry = Arc::new(ToolRegistry::new());
        host.register(&registry).unwrap();
        let count = registry.iter().len();

        assert!(host.register(&registry).is_err());
        assert_eq!(registry.iter().len(), count);
        assert_eq!(
            registry
                .iter()
                .iter()
                .filter(|entry| entry.name() == "file_index")
                .count(),
            1
        );
    }

    #[test]
    fn index_preflight_canonicalizes_file_and_directory_intents() {
        let root = TempDir::new().expect("tempdir");
        let file = root.path().join("source.rs");
        let directory = root.path().join("src");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        std::fs::create_dir(&directory).unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());

        for (path, kind, access) in [
            (
                file.as_path(),
                PermissionResourceKind::File,
                PermissionResourceAccess::Read,
            ),
            (
                directory.as_path(),
                PermissionResourceKind::Directory,
                PermissionResourceAccess::Search,
            ),
        ] {
            let invocation = registry
                .get("file_index")
                .unwrap()
                .tool
                .parse(&json!({"path": path}))
                .unwrap();
            let intent = smol::block_on(invocation.preflight(&ctx))
                .unwrap()
                .expect("permission intent");
            assert_eq!(intent.resources.len(), 1);
            assert_eq!(intent.resources[0].kind, kind);
            assert_eq!(intent.resources[0].access, Some(access));
            assert_eq!(
                Path::new(&intent.resources[0].value),
                path.canonicalize().unwrap()
            );
            assert_eq!(intent.risk, PermissionRisk::Low);
        }
    }

    #[test]
    fn index_relative_path_uses_project_root_when_process_cwd_differs() {
        let root = TempDir::new().expect("tempdir");
        let path = root.path().join("source.rs");
        std::fs::write(&path, "pub fn rooted() {}\n").unwrap();
        assert_ne!(
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            root.path().canonicalize().unwrap()
        );
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("file_index")
            .unwrap()
            .tool
            .parse(&json!({"path": "source.rs"}))
            .unwrap();

        let intent = smol::block_on(invocation.preflight(&ctx))
            .unwrap()
            .expect("permission intent");
        let result = smol::block_on(invocation.execute(&ctx));

        assert_eq!(
            Path::new(&intent.resources[0].value),
            path.canonicalize().unwrap()
        );
        assert_eq!(
            result.model_output.as_deref(),
            Some("fns:\n  pub rooted() [1]")
        );
    }

    #[test]
    fn index_file_preserves_model_and_structured_output_and_tracks_read() {
        let root = TempDir::new().expect("tempdir");
        let path = root.path().join("source.rs");
        std::fs::write(&path, "use std::io;\n\npub fn run() {}\n").unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("file_index")
            .unwrap()
            .tool
            .parse(&json!({"path": path}))
            .unwrap();

        let intent = smol::block_on(invocation.preflight(&ctx)).unwrap();
        assert!(intent.is_some());
        let result = smol::block_on(invocation.execute(&ctx));

        assert_eq!(
            result.model_output.as_deref(),
            Some("imports: [1]\n  std::io\n\nfns:\n  pub run() [3]")
        );
        let output = result.output.expect("index output");
        let state = output.state().expect("complete Workcell state");
        assert_eq!(state["kind"], "file");
        assert_eq!(state["language"], "rust");
        let ToolOutput::Index(AgentIndexOutput::File {
            path: output_path,
            language,
            skeleton,
            lines,
            ..
        }) = output
        else {
            panic!("expected native file index output")
        };
        assert_eq!(Path::new(&output_path), path.canonicalize().unwrap());
        assert_eq!(language, "rust");
        assert_eq!(skeleton, result.model_output.unwrap());
        assert_eq!(lines[0].semantic, AgentIndexLineSemantic::Section);

        bump_mtime(&path);
        assert!(ctx.file_tracker.check_before_edit(&path).is_err());
    }

    #[test]
    fn index_directory_preserves_listing_entries_without_tracking_directory() {
        let root = TempDir::new().expect("tempdir");
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "").unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("file_index")
            .unwrap()
            .tool
            .parse(&json!({"path": root.path()}))
            .unwrap();
        smol::block_on(invocation.preflight(&ctx)).unwrap();

        let result = smol::block_on(invocation.execute(&ctx));

        assert_eq!(result.model_output.as_deref(), Some("src/\nCargo.toml"));
        let output = result.output.expect("directory output");
        assert_eq!(output.state().unwrap()["kind"], "directory");
        let ToolOutput::Index(AgentIndexOutput::Directory {
            entries,
            total_count,
            listing,
            ..
        }) = output
        else {
            panic!("expected native directory index output")
        };
        assert_eq!(total_count, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(listing, "src/\nCargo.toml");

        std::fs::write(root.path().join("new.txt"), "new").unwrap();
        assert!(ctx.file_tracker.check_before_edit(root.path()).is_ok());
    }

    #[test]
    fn index_directory_truncation_is_explicit_and_state_stays_exact() {
        let output = WorkcellIndexOutput::Directory {
            path: "/project".into(),
            relative_path: ".".into(),
            entries: vec![workcell::files::IndexDirectoryEntry {
                name: "src".into(),
                kind: IndexDirectoryEntryKind::Directory,
            }],
            total_count: 2,
            truncated: true,
            listing: "src/".into(),
        };

        let result = index_result(output, IndexLimits::default().max_model_output_bytes);

        assert_eq!(result.model_output.as_deref(), Some("src/\n[truncated]"));
        let output = result.output.unwrap();
        assert_eq!(output.as_display_text(), "src/\n[truncated]");
        let state = output.state().unwrap();
        assert!(state.get("listing").is_none());
        assert_eq!(state["truncated"], true);

        let bounded = directory_listing_with_truncation("first\nsecond\nthird", true, 20);
        assert!(bounded.len() <= 20);
        assert!(bounded.ends_with(INDEX_TRUNCATED));
    }

    #[test]
    fn index_uses_configured_source_size_limit() {
        let root = TempDir::new().expect("tempdir");
        let path = root.path().join("large.rs");
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        ctx.config.index_max_file_size_mb = 1;
        let invocation = registry
            .get("file_index")
            .unwrap()
            .tool
            .parse(&json!({"path": path}))
            .unwrap();
        smol::block_on(invocation.preflight(&ctx)).unwrap();

        let result = smol::block_on(invocation.execute(&ctx));

        assert!(result.is_error);
        assert!(
            result
                .output
                .unwrap_err()
                .contains("exceeds maximum size of 1048576 bytes")
        );
    }

    #[test]
    fn registered_effects_match_workcell_boundaries() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());

        for (name, expected) in [
            ("file_read", ToolEffect::ReadOnly),
            ("file_glob", ToolEffect::ReadOnly),
            ("file_grep", ToolEffect::ReadOnly),
            ("file_index", ToolEffect::ReadOnly),
            ("websearch", ToolEffect::ReadOnly),
            ("webfetch", ToolEffect::ReadOnly),
            ("execution_environment", ToolEffect::Mutating),
            ("file_write", ToolEffect::Mutating),
            ("file_edit", ToolEffect::Mutating),
            ("file_apply_patch", ToolEffect::Mutating),
            ("shell", ToolEffect::Mutating),
        ] {
            assert_eq!(registry.get(name).unwrap().effect, expected, "{name}");
        }
        if let Some(code) = registry.get("python_execution") {
            assert_eq!(code.effect, ToolEffect::Isolated);
        }
    }

    #[test]
    fn read_only_dispatch_blocks_workcell_mutation_before_preflight() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context_with_mode(
            root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
            AgentMode::ReadOnly,
            DefaultEffect::Allow,
        );
        let target = root.path().join("forged.txt");

        let done = smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "write-read-only".into(),
            "file_write",
            &json!({"filePath": target, "content": "forged"}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ));

        assert!(done.is_error);
        assert!(done.output.as_text().contains("strict read-only mode"));
        assert!(!target.exists());
    }

    const PLAN_MARKER: &str = "planned";
    const PLAN_READ_COMMAND: &str = "ls";
    const UNCLASSIFIED_COMMAND: &str = "true";
    const UNCLASSIFIED_ALLOW_SCOPE: &str = "true";

    fn shell_in_plan_mode(command: &str) -> caudra_agent::ToolDoneEvent {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context_with_mode(
            root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
            AgentMode::Plan(root.path().join("plan.md")),
            DefaultEffect::Allow,
        );

        smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "shell-in-plan".into(),
            "shell",
            &json!({"command": command}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ))
    }

    /// Planning used to refuse every shell call, so a plan could not be
    /// researched with the tools the repository already trusts.
    #[test]
    fn planning_runs_a_read_only_shell_command() {
        let done = shell_in_plan_mode(PLAN_READ_COMMAND);

        assert!(!done.is_error, "{}", done.output.as_text());
    }

    /// The allowlist does not have to be exhaustive because what it misses is
    /// prompted rather than refused.
    #[test]
    fn planning_does_not_refuse_an_unclassified_shell_command_outright() {
        let done = shell_in_plan_mode(UNCLASSIFIED_COMMAND);
        let text = done.output.as_text();

        assert!(
            !text.contains(caudra_agent::tools::PLAN_WRITE_RESTRICTED),
            "{text}"
        );
    }

    /// A grant wide enough to cover the command must not carry it while
    /// planning: an "allow always" answered in build mode would otherwise let a
    /// plan run writes silently. A configured allow is authority the plan never
    /// asked for, so containment withholds it and the call has to ask. With no
    /// responder, asking means denied.
    #[test_case(AgentMode::Build => false ; "building_uses_the_configured_allow")]
    #[test_case(AgentMode::Plan(PathBuf::from("plan.md")) => true ; "planning_forces_the_prompt_anyway")]
    fn a_configured_shell_allow_does_not_reach_a_plan(mode: AgentMode) -> bool {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let (tx, _rx) = flume::unbounded::<Envelope>();
        let event_tx = EventSender::new(tx, 0);
        let permissions = PermissionManager::new_nonpersistent(
            PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: ToolKey::native("shell"),
                    scope: Some(UNCLASSIFIED_ALLOW_SCOPE.into()),
                    effect: Effect::Allow,
                }],
                ..PermissionsConfig::default()
            },
            root.path().to_path_buf(),
            Arc::default(),
        );
        let mut ctx = interpreter_ctx(
            &mode,
            &event_tx,
            CancelToken::none(),
            Arc::new(permissions),
            Arc::new(FileReadTracker::new()),
            None,
            Arc::clone(&registry),
        );
        ctx.config.stale_read_check = false;

        let done = smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "configured-shell".into(),
            "shell",
            &json!({"command": UNCLASSIFIED_COMMAND}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ));
        done.is_error
    }

    #[test]
    fn planning_still_refuses_a_write_outside_the_plan_file() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let target = root.path().join("forged.txt");
        let ctx = context_with_mode(
            root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
            AgentMode::Plan(root.path().join("plan.md")),
            DefaultEffect::Allow,
        );

        let done = smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "write-in-plan".into(),
            "file_write",
            &json!({"filePath": target, "content": PLAN_MARKER}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ));

        assert!(done.is_error);
        assert!(!target.exists());
    }

    fn glob_output(count: usize, total: usize, scan_complete: bool) -> FileGlobOutput {
        FileGlobOutput {
            cwd: "/project".into(),
            relative_path: ".".into(),
            pattern: "**/*.rs".into(),
            files: (0..count)
                .map(|index| workcell::files::FileListing {
                    path: format!("/project/{index}.rs"),
                    relative_path: format!("{index}.rs"),
                    size_bytes: None,
                    line_count: None,
                })
                .collect(),
            count,
            total,
            scan_complete,
            truncated: total > count || !scan_complete,
            ignored: 0,
            ignore_complete: true,
            pruned_repositories: Vec::new(),
        }
    }

    #[test_case(glob_output(2, 2, true),   "2 files"              ; "complete_scan_reports_what_it_found")]
    #[test_case(glob_output(1, 340, true), "1 of 340 files"       ; "a_result_cap_knows_the_total")]
    #[test_case(glob_output(1, 5, false),  "1 of at least 5 files" ; "an_early_stop_only_has_a_lower_bound")]
    #[test_case(glob_output(0, 0, false),  "0 files, scan capped" ; "an_early_stop_that_matched_nothing_quotes_no_total")]
    fn glob_annotation_distinguishes_a_capped_search_from_a_complete_one(
        output: FileGlobOutput,
        expected: &str,
    ) {
        assert_eq!(
            file_glob_result(output).annotation.as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn a_capped_glob_reports_what_it_withheld_to_both_readers() {
        let result = file_glob_result(glob_output(1, 340, true));
        let model = result.model_output.clone().expect("model text");
        assert!(
            model.contains("[truncated: showing 1 of 340 matching files]"),
            "{model}"
        );
        let ToolOutput::Plain(text) = result.output.unwrap() else {
            panic!("expected plain glob output");
        };
        // Workcell writes the notice, so restating it is what would make the
        // two renderings disagree.
        assert_eq!(text.text, model);
    }

    #[test]
    fn a_capped_glob_that_matched_nothing_is_not_a_complete_miss() {
        let ToolOutput::Plain(text) = file_glob_result(glob_output(0, 0, false)).output.unwrap()
        else {
            panic!("expected plain glob output");
        };
        assert_ne!(text.text, caudra_agent::NO_FILES_FOUND);
        assert!(text.text.contains("scan stopped early"), "{}", text.text);
    }

    #[test]
    fn a_capped_grep_carries_how_far_it_searched_into_the_result() {
        let result = file_grep_result(
            serde_json::from_value(serde_json::json!({
                "cwd": "/project",
                "relativePath": ".",
                "pattern": "needle",
                "include": null,
                "rows": [{
                    "path": "/project/a.rs",
                    "relativePath": "a.rs",
                    "line": 3,
                    "text": "needle",
                }],
                "matches": 1,
                "filesScanned": 40,
                "filesListed": 900,
                "truncated": true,
            }))
            .expect("grep fixture"),
        );
        let model = result.model_output.clone().expect("model text");
        assert!(
            model.contains("[truncated: showing 1 matches from 40 of 900 files searched]"),
            "{model}"
        );
        let ToolOutput::GrepResult { capped, .. } = result.output.unwrap() else {
            panic!("expected typed grep output");
        };
        let cap = capped.expect("a truncated search reports its reach");
        assert_eq!((cap.files_scanned, cap.files_listed), (40, 900));
    }

    #[test]
    fn file_results_use_syntax_aware_presentations() {
        let read = file_read_result(FileReadOutput::File {
            path: "/project/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            text: "fn main() {}".into(),
            numbered_text: "1: fn main() {}".into(),
            line_start: 1,
            line_end: 1,
            total_lines: 1,
            truncated: false,
        })
        .output
        .expect("read output");
        assert!(matches!(
            read,
            ToolOutput::ReadCode { path, .. } if path.ends_with("lib.rs")
        ));

        let grep = file_grep_result(
            serde_json::from_value(serde_json::json!({
                "cwd": "/project",
                "relativePath": ".",
                "pattern": "main",
                "include": "*.rs",
                "rows": [{
                    "path": "/project/src/lib.rs",
                    "relativePath": "src/lib.rs",
                    "line": 1,
                    "text": "fn main() {}",
                }],
                "matches": 1,
                "filesScanned": 1,
                "filesListed": 1,
                "truncated": false,
            }))
            .expect("grep fixture"),
        )
        .output
        .expect("grep output");
        assert!(matches!(
            grep,
            ToolOutput::GrepResult { entries, capped: None }
                if entries.first().is_some_and(|entry| entry.path == "src/lib.rs")
        ));

        let diff = workcell::files::FileDiff {
            file: "/project/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            patch: "-fn old() {}\n+fn new() {}".into(),
            additions: 1,
            deletions: 1,
            truncated: false,
        };
        let write = file_write_result(
            FileWriteOutput {
                kind: workcell::files::FileWriteKind::Write,
                path: "/project/src/lib.rs".into(),
                relative_path: "src/lib.rs".into(),
                existed: false,
                applied: true,
                diff: diff.clone(),
                previous: None,
            },
            "fn new() {}\n".into(),
        )
        .output
        .expect("write output");
        assert!(matches!(
            write,
            ToolOutput::WriteCode { path, .. } if path.ends_with("lib.rs")
        ));

        let edit = file_edit_result(
            FileEditOutput {
                kind: workcell::files::FileEditKind::Edit,
                path: "/project/src/lib.rs".into(),
                relative_path: "src/lib.rs".into(),
                applied: true,
                diff,
            },
            "fn old() {}".into(),
            "fn new() {}".into(),
            false,
        )
        .output
        .expect("edit output");
        assert!(matches!(
            edit,
            ToolOutput::Diff { path, .. } if path.ends_with("lib.rs")
        ));

        let patch = file_patch_result(FileApplyPatchOutput {
            kind: workcell::files::FilePatchKind::Patch,
            applied: false,
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs".into(),
            files: vec![workcell::files::FileMutation {
                file_path: "/project/src/lib.rs".into(),
                relative_path: "src/lib.rs".into(),
                mutation_type: workcell::files::FileMutationType::Update,
                patch: "@@ -1,1 +1,1 @@\n-old\n+new".into(),
                additions: 1,
                deletions: 1,
                truncated: false,
                move_path: None,
            }],
            truncated: false,
        })
        .output
        .expect("patch output");
        assert!(matches!(
            patch,
            ToolOutput::Patch { files } if files.len() == 1 && files[0].path == "src/lib.rs"
        ));
    }

    const OLD_CONTENT: &str = "fn old() {}\n";
    const NEW_CONTENT: &str = "fn new() {}\n";
    const WRITE_CODE_CARD: &str = "write-code";
    const DIFF_CARD: &str = "diff";
    const PATCH_CARD: &str = "patch";

    fn write_result(applied: bool, existed: bool, previous: Option<&str>) -> ToolOutput {
        file_write_result(
            FileWriteOutput {
                kind: workcell::files::FileWriteKind::Write,
                path: "/project/src/lib.rs".into(),
                relative_path: "src/lib.rs".into(),
                existed,
                applied,
                diff: workcell::files::FileDiff {
                    file: "/project/src/lib.rs".into(),
                    relative_path: "src/lib.rs".into(),
                    patch: "@@ -1,1 +1,1 @@\n-fn old() {}\n+fn new() {}".into(),
                    additions: 1,
                    deletions: 1,
                    truncated: false,
                },
                previous: previous.map(str::to_owned),
            },
            NEW_CONTENT.into(),
        )
        .output
        .expect("write output")
    }

    /// A create is its content and an overwrite is a change, so the card a
    /// write settles into is decided by what the path held before it ran.
    #[test_case(true, true, Some(OLD_CONTENT) => DIFF_CARD ; "an_overwrite_shows_the_side_it_replaced")]
    #[test_case(true, false, None => WRITE_CODE_CARD ; "a_new_file_has_no_other_side_to_show")]
    #[test_case(true, true, None => PATCH_CARD ; "an_old_side_too_large_to_carry_falls_back_to_the_patch")]
    #[test_case(false, true, Some(OLD_CONTENT) => PATCH_CARD ; "a_write_that_never_landed_reports_only_the_patch")]
    fn a_write_is_drawn_from_what_the_path_held_before_it(
        applied: bool,
        existed: bool,
        previous: Option<&str>,
    ) -> &'static str {
        match write_result(applied, existed, previous) {
            ToolOutput::WriteCode { .. } => WRITE_CODE_CARD,
            ToolOutput::Diff { before, after, .. } => {
                assert_eq!(before, OLD_CONTENT);
                assert_eq!(after, NEW_CONTENT);
                DIFF_CARD
            }
            ToolOutput::Patch { .. } => PATCH_CARD,
            other => panic!("unexpected write output: {other:?}"),
        }
    }

    #[test]
    fn production_reserves_python_execution_when_an_override_is_invalid() {
        let root = TempDir::new().expect("tempdir");
        let missing = root.path().join("missing-worker");
        let host =
            WorkcellHost::new_production(root.path(), Some(&missing)).expect("Workcell host");
        let registry = Arc::new(ToolRegistry::new());
        host.register(&registry).expect("Workcell registration");

        assert_eq!(registry.iter().len(), NATIVE_TOOL_NAMES.len());
        assert!(registry.get("python_execution").is_some());
        assert!(
            host.warnings()
                .iter()
                .any(|warning| warning.contains("python_execution"))
        );
    }

    /// A write asks for write access and names the file it would change, so
    /// the permission layer can prompt before anything is touched.
    #[test]
    fn file_write_has_write_intent_and_a_mutation_target() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("file_write")
            .expect("registered file write")
            .tool
            .parse(&json!({ "filePath": "target.txt", "content": "body" }))
            .expect("valid write input");

        let intent = smol::block_on(invocation.preflight(&ctx))
            .expect("write preflight")
            .expect("permission intent");

        assert_eq!(intent.resources.len(), 1);
        assert_eq!(
            intent.resources[0].access,
            Some(PermissionResourceAccess::Write)
        );
        assert!(!invocation.mutation_targets(&ctx).is_empty());
        assert!(
            invocation.read_targets(&ctx).is_empty(),
            "{EXPECT_NO_DOUBLE_GUARD}"
        );
    }

    const CONTENT_FILE: &str = "readable.txt";
    const EXPECT_NO_DOUBLE_GUARD: &str =
        "a mutation target is never also a read target, or the call would block on itself";
    const EXPECT_READ_GUARD: &str = "a whole-file read is guarded, so a concurrent write cannot land between the \
         content and the mtime recorded for it";
    const EXPECT_NO_COARSE_GUARD: &str =
        "a search names no file up front, so it declares nothing to guard";

    /// `file_read` and `file_index` record the file's mtime only after reading its
    /// content. Without a shared guard a write landing in between records an
    /// mtime newer than what the model saw, and the next edit passes its stale
    /// check holding stale content.
    #[test_case("file_read", json!({ "filePath": CONTENT_FILE }), true ; "file_read_guards_its_file")]
    #[test_case("file_index", json!({ "path": CONTENT_FILE }), true ; "index_guards_its_file")]
    #[test_case("file_index", json!({ "path": "." }), false ; "index_does_not_guard_a_directory")]
    #[test_case("file_grep", json!({ "pattern": "body" }), false ; "grep_guards_nothing")]
    #[test_case("file_glob", json!({ "pattern": "*.txt" }), false ; "glob_guards_nothing")]
    fn read_targets_cover_whole_file_reads_only(tool: &str, input: Value, guarded: bool) {
        let root = TempDir::new().expect("tempdir");
        std::fs::write(root.path().join(CONTENT_FILE), "body").expect("seed file");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get(tool)
            .expect("registered tool")
            .tool
            .parse(&input)
            .expect("valid input");

        smol::block_on(invocation.preflight(&ctx)).expect("preflight");

        let expected: &[PathBuf] = &[root.path().join(CONTENT_FILE)];
        let reason = if guarded {
            EXPECT_READ_GUARD
        } else {
            EXPECT_NO_COARSE_GUARD
        };
        assert_eq!(
            invocation.read_targets(&ctx) == expected,
            guarded,
            "{reason}"
        );
        assert!(
            invocation.mutation_targets(&ctx).is_empty(),
            "{EXPECT_NO_DOUBLE_GUARD}"
        );
    }

    const REMOTE_CWD: &str = "project";
    const REMOTE_TARGET: &str = "/project/src/lib.rs";
    const REMOTE_MOVED: &str = "/project/src/moved.rs";
    const REMOTE_ABSOLUTE: &str = "/workspace/project/src/lib.rs";
    const REMOTE_MOVE_PATCH: &str =
        "*** Begin Patch\n*** Update File: src/lib.rs\n*** Move to: src/moved.rs\n*** End Patch";

    /// A remote write is prepared against the file as the host sees it, so its
    /// keys name that file from the workspace root however a relative call
    /// spells it. Shell, python and reads name nothing, exactly as they do
    /// locally.
    #[test_case(ToolKind::FileWrite, json!({"filePath": "src/lib.rs", "content": ""}), &[REMOTE_TARGET] ; "relative_write")]
    #[test_case(ToolKind::FileEdit, json!({"filePath": "./src/../src/lib.rs", "oldString": "a", "newString": "b"}), &[REMOTE_TARGET] ; "folded_edit")]
    #[test_case(ToolKind::FileEdit, json!({"filePath": REMOTE_ABSOLUTE, "oldString": "a", "newString": "b"}), &[REMOTE_ABSOLUTE] ; "absolute_edit_keeps_its_spelling")]
    #[test_case(ToolKind::FileApplyPatch, json!({"patchText": REMOTE_MOVE_PATCH}), &[REMOTE_TARGET, REMOTE_MOVED] ; "patch_names_every_file")]
    #[test_case(ToolKind::Shell, json!({"command": "echo > src/lib.rs"}), &[] ; "shell_names_nothing")]
    #[test_case(ToolKind::Code, json!({"code": "1"}), &[] ; "python_names_nothing")]
    #[test_case(ToolKind::FileRead, json!({"filePath": "src/lib.rs"}), &[] ; "read_names_nothing")]
    fn remote_writes_lock_the_host_files_they_prepare(
        kind: ToolKind,
        input: Value,
        expected: &[&str],
    ) {
        let input = Input::parse(kind, input).expect("valid input");
        let expected = expected
            .iter()
            .map(|path| LockKey::Remote((*path).to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(remote_write_keys(&input, REMOTE_CWD), expected);
    }

    #[test_case(ToolKind::FileWrite, json!({"filePath": "src/lib.rs", "content": ""}) => recorded(&["project/src/lib.rs"]) ; "a_relative_write_lands_under_the_cursor")]
    #[test_case(ToolKind::FileApplyPatch, json!({"patchText": REMOTE_MOVE_PATCH}) => recorded(&["project/src/lib.rs", "project/src/moved.rs"]) ; "a_patch_records_every_file")]
    #[test_case(ToolKind::FileEdit, json!({"filePath": REMOTE_ABSOLUTE, "oldString": "a", "newString": "b"}) => Some(RecordScope::Workspace) ; "an_absolute_write_is_unplaced")]
    #[test_case(ToolKind::FileEdit, json!({"filePath": "./src/../src/lib.rs", "oldString": "a", "newString": "b"}) => Some(RecordScope::Workspace) ; "a_parent_step_is_unplaced")]
    #[test_case(ToolKind::Code, json!({"code": "1"}) => Some(RecordScope::Workspace) ; "a_call_naming_no_file_records_everything")]
    fn a_remote_write_records_the_files_it_names(
        kind: ToolKind,
        input: Value,
    ) -> Option<RecordScope> {
        let input = Input::parse(kind, input).expect("valid input");
        Some(remote_write_scope(&input, REMOTE_CWD))
    }

    const REMOTE_RELATIVE_TARGET: &str = "src/lib.rs";
    const REMOTE_MOVE_PATCH_FILES: &str = "src/lib.rs, src/moved.rs";
    const HOST_REFUSAL: &str = "Prepared resource changed before publication: src/lib.rs";
    const OTHER_FAILURE_CODE: &str = "filesystem_io";

    /// The host refuses a write whose file changed after it was prepared, and
    /// publishes nothing, so the agent is told to read the file again as it is
    /// locally. Any other failure keeps the host's words.
    #[test_case(ToolKind::FileWrite, json!({"filePath": REMOTE_RELATIVE_TARGET, "content": ""}), STALE_RESOURCE_CODE, Some(REMOTE_RELATIVE_TARGET) ; "stale_write")]
    #[test_case(ToolKind::FileEdit, json!({"filePath": REMOTE_ABSOLUTE, "oldString": "a", "newString": "b"}), STALE_RESOURCE_CODE, Some(REMOTE_ABSOLUTE) ; "stale_edit_keeps_its_spelling")]
    #[test_case(ToolKind::FileApplyPatch, json!({"patchText": REMOTE_MOVE_PATCH}), STALE_RESOURCE_CODE, Some(REMOTE_MOVE_PATCH_FILES) ; "stale_patch_names_every_file")]
    #[test_case(ToolKind::FileWrite, json!({"filePath": REMOTE_RELATIVE_TARGET, "content": ""}), OTHER_FAILURE_CODE, None ; "other_write_failure")]
    #[test_case(ToolKind::Shell, json!({"command": "true"}), STALE_RESOURCE_CODE, None ; "shell_keeps_the_host_message")]
    fn a_stale_remote_write_asks_for_a_re_read(
        kind: ToolKind,
        input: Value,
        code: &str,
        reread: Option<&str>,
    ) {
        let input = Input::parse(kind, input).expect("valid input");
        let error = OperationError {
            code: OperationId::new(code).unwrap(),
            message: HOST_REFUSAL.to_owned(),
        };
        let expected = reread.map_or_else(|| HOST_REFUSAL.to_owned(), stale_read_message);
        assert_eq!(remote_failure_message(&input, error), expected);
    }

    /// A host refuses with Workcell's own code, so a file failure lands in the
    /// same bucket whether the tool ran here or on the host.
    #[test_case(FilesystemError::RootEscape(HOST_REFUSAL.into()), ToolFailure::Denied ; "outside_the_root")]
    #[test_case(FilesystemError::ProtectedPath(HOST_REFUSAL.into()), ToolFailure::Denied ; "a_protected_path")]
    #[test_case(FilesystemError::NotFound(HOST_REFUSAL.into()), ToolFailure::NotFound ; "a_missing_path")]
    #[test_case(FilesystemError::Io { context: HOST_REFUSAL.into(), source: ErrorKind::PermissionDenied.into() }, ToolFailure::Denied ; "a_permission_error")]
    #[test_case(FilesystemError::Aborted, ToolFailure::Cancelled ; "an_aborted_operation")]
    #[test_case(FilesystemError::Stale(HOST_REFUSAL.into()), ToolFailure::Other ; "a_stale_resource")]
    #[test_case(FilesystemError::Invalid(HOST_REFUSAL.into()), ToolFailure::InvalidInput ; "a_refused_request")]
    #[test_case(FilesystemError::Operation(HOST_REFUSAL.into()), ToolFailure::Other ; "a_failed_operation")]
    fn a_file_failure_lands_in_one_bucket_locally_and_remotely(
        error: FilesystemError,
        failure: ToolFailure,
    ) {
        let input = Input::parse(
            ToolKind::FileRead,
            json!({"filePath": REMOTE_RELATIVE_TARGET}),
        )
        .expect("valid input");
        let remote = remote_failure_result(
            &input,
            OperationError {
                code: OperationId::new(error.code()).unwrap(),
                message: HOST_REFUSAL.to_owned(),
            },
        );

        assert_eq!(remote.failure, Some(failure));
        assert_eq!(filesystem_error(error).failure, failure);
    }

    /// Web and shell refusals are placed by the same code a host reports them
    /// with, so a timeout or a blocked target never reads as a plain failure.
    #[test_case(webfetch_error(WebfetchError::InvalidInput(HOST_REFUSAL.into())), ToolFailure::InvalidInput ; "a_malformed_url")]
    #[test_case(webfetch_error(WebfetchError::Aborted), ToolFailure::Cancelled ; "an_aborted_fetch")]
    #[test_case(webfetch_error(WebfetchError::TimedOut(HOST_REFUSAL.into())), ToolFailure::Timeout ; "a_timed_out_fetch")]
    #[test_case(webfetch_error(WebfetchError::Denied(HOST_REFUSAL.into())), ToolFailure::Denied ; "a_blocked_target")]
    #[test_case(webfetch_error(WebfetchError::NotFound(HOST_REFUSAL.into())), ToolFailure::NotFound ; "a_missing_page")]
    #[test_case(webfetch_error(WebfetchError::Operation(HOST_REFUSAL.into())), ToolFailure::Other ; "a_failed_fetch")]
    #[test_case(shell_preparation_error(ShellPreparationError::OutsideRoot(HOST_REFUSAL.into())), ToolFailure::Denied ; "a_workdir_outside_the_root")]
    #[test_case(shell_preparation_error(ShellPreparationError::NotFound(HOST_REFUSAL.into())), ToolFailure::NotFound ; "a_missing_workdir")]
    #[test_case(shell_preparation_error(ShellPreparationError::Invalid(HOST_REFUSAL.into())), ToolFailure::InvalidInput ; "an_invalid_command")]
    fn a_web_or_shell_refusal_lands_in_the_bucket_its_code_names(
        error: ToolError,
        failure: ToolFailure,
    ) {
        assert_eq!(error.failure, failure);
    }

    /// A host reports only that an operation was cancelled, so it is a timeout
    /// exactly when Caudra's own deadline passed and its caller did not cancel.
    #[test_case(false, true => ToolFailure::Timeout ; "past_the_deadline")]
    #[test_case(false, false => ToolFailure::Cancelled ; "before_the_deadline")]
    #[test_case(true, true => ToolFailure::Cancelled ; "a_caller_that_cancelled_wins")]
    fn a_remote_cancellation_is_a_timeout_only_past_the_deadline(
        cancelled: bool,
        expired: bool,
    ) -> ToolFailure {
        let root = TempDir::new().unwrap();
        let (trigger, cancel) = CancelToken::new();
        let ctx = context(root.path(), Arc::new(ToolRegistry::new()), cancel);
        if cancelled {
            trigger.cancel();
        }
        let now = Instant::now();
        let deadline = if expired {
            now
        } else {
            now + REMOTE_EXECUTION_TIMEOUT
        };
        remote_cancellation(&ctx, deadline)
    }

    /// Workcell records every successful `file_read` against the tracker it is
    /// handed, `offset` or not, so the only thing standing between a 20-line
    /// mention and a later edit passing its staleness check on the whole file
    /// is the throwaway tracker the resolver gives a slice.
    #[test_case(None, true ; "whole_file_records_the_read")]
    #[test_case(Some(1..=1), false ; "a_slice_records_nothing")]
    fn a_mention_records_a_read_only_when_it_covers_the_file(
        lines: Option<RangeInclusive<usize>>,
        recorded: bool,
    ) {
        let root = TempDir::new().expect("tempdir");
        let path = root.path().join(CONTENT_FILE);
        std::fs::write(&path, "first\nsecond\n").expect("seed file");
        let (_host, registry) = host_and_registry(root.path());

        let mut whole_file = context(root.path(), Arc::clone(&registry), CancelToken::none());
        whole_file.config.stale_read_check = true;
        let slice = ToolContext {
            file_tracker: FileReadTracker::fresh(),
            ..context(root.path(), Arc::clone(&registry), CancelToken::none())
        };
        let tracker = Arc::clone(&whole_file.file_tracker);

        let mut budget = mention_preamble::MAX_TOTAL_BYTES;
        let messages = smol::block_on(mention_preamble::build(
            &[Mention::new(CONTENT_FILE, lines)],
            mention_preamble::Resolution {
                root: root.path(),
                registry: &registry,
                whole_file: &whole_file,
                slice: &slice,
                vision: false,
                remote_context: None,
            },
            &mut budget,
        ));

        assert_eq!(messages.len(), 1);
        assert!(
            message_text(&messages[0]).contains("first"),
            "{EXPECT_MENTION_INLINED}"
        );
        bump_mtime(&path);
        assert_eq!(
            tracker.check_before_edit(&path).is_err(),
            recorded,
            "{EXPECT_SLICE_LEAVES_NO_RECORD}"
        );
    }

    #[test]
    fn a_mention_of_a_missing_file_says_so_instead_of_going_quiet() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());

        let mut budget = mention_preamble::MAX_TOTAL_BYTES;
        let messages = smol::block_on(mention_preamble::build(
            &[Mention::new("absent.txt", None)],
            mention_preamble::Resolution {
                root: root.path(),
                registry: &registry,
                whole_file: &ctx,
                slice: &ctx,
                vision: false,
                remote_context: None,
            },
            &mut budget,
        ));

        let text = message_text(&messages[0]);
        assert!(text.contains("error="), "{EXPECT_MENTION_NOTE}: {text}");
        assert_eq!(
            messages[0].display_text.as_deref(),
            Some(""),
            "{EXPECT_MENTION_HIDDEN}"
        );
    }

    fn message_text(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Write authority is Caudra's to grant, so a model cannot ask for a
    /// preview that skips it. The flag that used to do so is now an unknown
    /// argument, and an unknown argument fails the call instead of writing.
    #[test]
    fn a_mutation_tool_rejects_a_model_supplied_preview_flag() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        assert!(
            registry
                .get("file_write")
                .expect("registered file write")
                .tool
                .parse(&json!({
                    "filePath": "preview.txt",
                    "content": "preview",
                    "dryRun": true
                }))
                .is_err(),
            "{PREVIEW_FLAG_MSG}"
        );
    }

    #[test]
    fn expired_caller_deadline_stops_preflight() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let mut ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        ctx.deadline = Deadline::after(Duration::ZERO);
        let invocation = registry
            .get("file_read")
            .expect("registered file read")
            .tool
            .parse(&json!({"filePath": "missing.txt"}))
            .expect("valid read input");

        let error = smol::block_on(invocation.preflight(&ctx)).unwrap_err();
        assert_eq!(error.message, DEADLINE_EXCEEDED);
        assert_eq!(error.failure, ToolFailure::Timeout);
    }

    /// An operation stopped by the caller's deadline reports the deadline, not
    /// whatever the stopped operation returned on its way out.
    #[test]
    fn a_deadline_reached_mid_operation_is_a_timeout() {
        let root = TempDir::new().expect("tempdir");
        let (host, registry) = host_and_registry(root.path());
        let mut ctx = context(root.path(), registry, CancelToken::none());
        ctx.deadline = Deadline::after(RUN_DEADLINE);

        let error = smol::block_on(
            host.inner
                .run(&ctx, |token| async move { token.cancelled().await }),
        )
        .expect_err("only the deadline ends the operation");

        assert_eq!(error.message, DEADLINE_EXCEEDED);
        assert_eq!(error.failure, ToolFailure::Timeout);
    }

    #[cfg(unix)]
    #[test]
    fn file_execution_stays_bound_to_the_path_authorized_during_preflight() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let authorized = root.path().join("authorized.txt");
        let unauthorized = outside.path().join("unauthorized.txt");
        let link = root.path().join("link.txt");
        std::fs::write(&authorized, "authorized").unwrap();
        std::fs::write(&unauthorized, "unauthorized").unwrap();
        symlink(&authorized, &link).unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("file_read")
            .expect("registered file read")
            .tool
            .parse(&json!({"filePath": "link.txt"}))
            .expect("valid read input");

        smol::block_on(invocation.preflight(&ctx)).expect("read preflight");
        std::fs::remove_file(&link).unwrap();
        symlink(&unauthorized, &link).unwrap();
        let result = smol::block_on(invocation.execute(&ctx));

        let output = result.output.expect("successful read");
        let ToolOutput::ReadCode { path, lines, .. } = output else {
            panic!("expected syntax-aware read output");
        };
        assert_eq!(path, authorized.to_string_lossy());
        assert_eq!(lines, ["authorized"]);
    }

    #[test_case("*.rs"; "basename_glob_is_recursive")]
    #[test_case("**/*.rs"; "explicit_recursive_glob")]
    #[test_case("{visible,nested/item}.rs"; "brace_glob_is_recursive")]
    fn browsing_glob_preflight_is_names_only_and_explicitly_recursive(pattern: &str) {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("visible.rs"), BROWSE_CONTENT_SENTINEL).unwrap();
        std::fs::write(root.path().join("nested/item.rs"), BROWSE_CONTENT_SENTINEL).unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let input = json!({"pattern": pattern});
        let invocation = registry
            .get("file_glob")
            .unwrap()
            .tool
            .parse(&input)
            .unwrap();
        let intent = smol::block_on(invocation.preflight(&ctx)).unwrap().unwrap();
        assert_eq!(intent.resources[0].kind, PermissionResourceKind::Directory);
        assert_eq!(
            intent.resources[0].access,
            Some(PermissionResourceAccess::List)
        );
        assert_eq!(
            intent.resources[0].attributes[BROWSE_RECURSION_ATTRIBUTE],
            BROWSE_RECURSIVE
        );
        assert_eq!(
            Path::new(&intent.resources[0].value),
            root.path().canonicalize().unwrap()
        );
        let request = PermissionRequest::from_intent_with_identity(
            "browsing".into(),
            ToolKey::native("file_glob"),
            &intent,
            input,
            root.path(),
            PermissionSubject::Native {
                owner: OWNER.into(),
                contract: "file.glob.v1".into(),
            },
            PermissionExecutorKind::Native,
        );
        assert!(
            request
                .options
                .iter()
                .any(|option| option.rule.family
                    == Some(PermissionCapabilityFamily::FilesystemBrowse))
        );
        assert!(
            request.options.iter().all(
                |option| option.rule.family != Some(PermissionCapabilityFamily::FilesystemRead)
            )
        );
        let result = smol::block_on(invocation.execute(&ctx));
        let text = result.output.unwrap().as_text();
        assert!(text.contains("nested/item.rs"));
        assert!(!text.contains(BROWSE_CONTENT_SENTINEL));
    }

    #[test]
    fn browsing_directory_grants_are_direct_unless_recursion_is_explicitly_selected() {
        let root = TempDir::new().unwrap();
        let directory = root.path().join("browse");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(directory.join("nested")).unwrap();
        std::fs::write(directory.join("visible.rs"), BROWSE_CONTENT_SENTINEL).unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let request = |tool: &str, contract: &str, input: Value| {
            let invocation = registry.get(tool).unwrap().tool.parse(&input).unwrap();
            let intent = smol::block_on(invocation.preflight(&ctx)).unwrap().unwrap();
            PermissionRequest::from_intent_with_identity(
                "browsing".into(),
                ToolKey::native(tool),
                &intent,
                input,
                root.path(),
                PermissionSubject::Native {
                    owner: OWNER.into(),
                    contract: contract.into(),
                },
                PermissionExecutorKind::Native,
            )
        };
        let listing = request("file_read", "file.read.v1", json!({"filePath": "browse"}));
        assert_eq!(listing.resources[0].kind, PermissionResourceKind::Directory);
        assert_eq!(
            listing.resources[0].access,
            Some(PermissionResourceAccess::List)
        );
        assert_eq!(
            listing.resources[0].attributes[BROWSE_RECURSION_ATTRIBUTE],
            BROWSE_DIRECT
        );
        assert_eq!(
            Path::new(&listing.resources[0].value),
            directory.canonicalize().unwrap()
        );
        assert!(
            listing.options.iter().all(
                |option| option.rule.family != Some(PermissionCapabilityFamily::FilesystemRead)
            )
        );
        let direct_option = listing
            .options
            .iter()
            .find(|option| {
                option.rule.family == Some(PermissionCapabilityFamily::FilesystemBrowse)
                    && matches!(
                        option.rule.resources[0].selector,
                        PermissionResourceSelector::Digest { .. }
                    )
            })
            .unwrap();
        let direct = listing
            .option_rule(&direct_option.id, PermissionLifetime::Conversation)
            .unwrap();
        assert!(permission_rule_covers_request(&direct, &listing));
        let glob = request(
            "file_glob",
            "file.glob.v1",
            json!({"path": "browse", "pattern": "*.rs"}),
        );
        let nested = request(
            "file_read",
            "file.read.v1",
            json!({"filePath": "browse/nested"}),
        );
        let content = request(
            "file_read",
            "file.read.v1",
            json!({"filePath": "browse/visible.rs"}),
        );
        for outside_direct_scope in [&glob, &nested, &content] {
            assert!(!permission_rule_covers_request(
                &direct,
                outside_direct_scope
            ));
        }
        let recursive_option = listing
            .options
            .iter()
            .find(|option| {
                option.rule.family == Some(PermissionCapabilityFamily::FilesystemBrowse)
                    && matches!(
                        option.rule.resources[0].selector,
                        PermissionResourceSelector::FilesystemSubtreeDigest { .. }
                    )
            })
            .unwrap();
        let recursive = listing
            .option_rule(&recursive_option.id, PermissionLifetime::Conversation)
            .unwrap();
        assert!(permission_rule_covers_request(&recursive, &glob));
        assert!(permission_rule_covers_request(&recursive, &nested));
        assert!(!permission_rule_covers_request(&recursive, &content));
    }

    #[test_case(true; "directory_replaced_by_file")]
    #[test_case(false; "file_replaced_by_directory")]
    fn browsing_read_execution_refuses_a_kind_change_after_preflight(directory: bool) {
        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        if directory {
            std::fs::create_dir(&target).unwrap();
        } else {
            std::fs::write(&target, BROWSE_CONTENT_SENTINEL).unwrap();
        }
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let input = json!({"filePath": "target"});
        let invocation = registry
            .get("file_read")
            .unwrap()
            .tool
            .parse(&input)
            .unwrap();
        let intent = smol::block_on(invocation.preflight(&ctx)).unwrap().unwrap();
        assert_eq!(
            intent.resources[0].access,
            Some(if directory {
                PermissionResourceAccess::List
            } else {
                PermissionResourceAccess::Read
            })
        );
        if directory {
            std::fs::remove_dir(&target).unwrap();
            std::fs::write(&target, BROWSE_CONTENT_SENTINEL).unwrap();
        } else {
            std::fs::remove_file(&target).unwrap();
            std::fs::create_dir(&target).unwrap();
        }
        assert!(smol::block_on(invocation.execute(&ctx)).output.is_err());
        let fresh = registry
            .get("file_read")
            .unwrap()
            .tool
            .parse(&input)
            .unwrap();
        let intent = smol::block_on(fresh.preflight(&ctx)).unwrap().unwrap();
        assert_eq!(
            intent.resources[0].access,
            Some(if directory {
                PermissionResourceAccess::Read
            } else {
                PermissionResourceAccess::List
            })
        );
    }

    #[cfg(unix)]
    #[test_case(false; "retargeted_to_file")]
    #[test_case(true; "retargeted_to_directory")]
    fn browsing_directory_execution_keeps_the_canonical_preflight_target(directory: bool) {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let authorized = root.path().join("authorized");
        let link = root.path().join("alias");
        std::fs::create_dir(&authorized).unwrap();
        std::fs::write(authorized.join("visible.rs"), BROWSE_CONTENT_SENTINEL).unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, BROWSE_CONTENT_SENTINEL).unwrap();
        symlink(&authorized, &link).unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("file_read")
            .unwrap()
            .tool
            .parse(&json!({"filePath": "alias"}))
            .unwrap();
        let intent = smol::block_on(invocation.preflight(&ctx)).unwrap().unwrap();
        assert_eq!(
            intent.resources[0].access,
            Some(PermissionResourceAccess::List)
        );
        assert!(invocation.read_targets(&ctx).is_empty());
        std::fs::remove_file(&link).unwrap();
        symlink(
            if directory {
                outside.path()
            } else {
                secret.as_path()
            },
            &link,
        )
        .unwrap();
        let output = smol::block_on(invocation.execute(&ctx))
            .output
            .unwrap()
            .as_text();
        assert!(output.contains("visible.rs"));
        assert!(!output.contains("secret.txt"));
        assert!(!output.contains(BROWSE_CONTENT_SENTINEL));
    }

    #[test_case("file_read", json!({"filePath": "source.rs"}), PermissionResourceKind::File, PermissionResourceAccess::Read; "file_read")]
    #[test_case("file_grep", json!({"path": ".", "pattern": "fn"}), PermissionResourceKind::Directory, PermissionResourceAccess::Search; "grep")]
    #[test_case("file_index", json!({"path": "."}), PermissionResourceKind::Directory, PermissionResourceAccess::Search; "directory_index")]
    #[test_case("file_index", json!({"path": "source.rs"}), PermissionResourceKind::File, PermissionResourceAccess::Read; "source_index")]
    fn browsing_classification_does_not_relabel_other_read_contracts(
        tool: &str,
        input: Value,
        kind: PermissionResourceKind,
        access: PermissionResourceAccess,
    ) {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("source.rs"), "fn main() {}\n").unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry.get(tool).unwrap().tool.parse(&input).unwrap();
        let intent = smol::block_on(invocation.preflight(&ctx)).unwrap().unwrap();
        assert_eq!(intent.resources[0].kind, kind);
        assert_eq!(intent.resources[0].access, Some(access));
        assert!(
            !intent.resources[0]
                .attributes
                .contains_key(BROWSE_RECURSION_ATTRIBUTE)
        );
    }

    #[cfg(unix)]
    #[test_case("file_glob", false; "glob_excludes_symlinks_and_protected_descendants")]
    #[test_case("file_glob", true; "glob_refuses_a_substituted_root")]
    #[test_case("file_read", false; "listing_excludes_symlinks_and_protected_descendants")]
    #[test_case("file_read", true; "listing_refuses_a_substituted_root")]
    fn browsing_stays_inside_its_policy_root(tool: &str, substitute_root: bool) {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let directory = root.path().join("browse");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("visible.rs"), BROWSE_CONTENT_SENTINEL).unwrap();
        std::fs::write(directory.join(".env"), BROWSE_CONTENT_SENTINEL).unwrap();
        std::fs::create_dir(directory.join(".ssh")).unwrap();
        std::fs::write(directory.join(".ssh/secret.rs"), BROWSE_CONTENT_SENTINEL).unwrap();
        std::fs::write(
            outside.path().join("outside_secret.rs"),
            BROWSE_CONTENT_SENTINEL,
        )
        .unwrap();
        symlink(outside.path(), directory.join("link")).unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let input = if tool == "file_read" {
            json!({"filePath": "browse"})
        } else {
            json!({"path": "browse", "pattern": "**/*"})
        };
        let invocation = registry.get(tool).unwrap().tool.parse(&input).unwrap();
        smol::block_on(invocation.preflight(&ctx)).unwrap();
        if substitute_root {
            std::fs::rename(&directory, root.path().join("original")).unwrap();
            symlink(outside.path(), &directory).unwrap();
        }
        let result = smol::block_on(invocation.execute(&ctx));
        if substitute_root {
            assert!(result.output.is_err());
        } else {
            let text = result.output.unwrap().as_text();
            assert!(text.contains("visible.rs"));
            assert!(!text.contains("secret.rs"));
            assert!(!text.contains(".env"));
            assert!(!text.contains(BROWSE_CONTENT_SENTINEL));
        }
    }

    #[test]
    fn broad_grep_excludes_protected_files() {
        let root = TempDir::new().expect("tempdir");
        std::fs::write(root.path().join("visible.txt"), "needle").unwrap();
        std::fs::write(root.path().join(".env"), "SECRET=needle").unwrap();
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("file_grep")
            .expect("registered file grep")
            .tool
            .parse(&json!({"pattern": "needle", "path": "."}))
            .expect("valid grep input");

        smol::block_on(invocation.preflight(&ctx)).expect("grep preflight");
        let result = smol::block_on(invocation.execute(&ctx));
        let output = result.output.expect("successful grep");
        let ToolOutput::GrepResult { entries, .. } = output else {
            panic!("expected syntax-aware grep output");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "visible.txt");
        assert_eq!(entries[0].groups[0].lines[0].text, "needle");
    }

    #[test]
    fn execution_environment_uses_the_active_session_working_directory() {
        let startup_root = TempDir::new().expect("startup tempdir");
        let session_root = TempDir::new().expect("session tempdir");
        std::fs::write(session_root.path().join(LOCKFILE), "{}").unwrap();
        let (_host, registry) = host_and_registry(startup_root.path());
        let ctx = context(
            session_root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
        );
        let invocation = registry
            .get("execution_environment")
            .expect("registered environment")
            .tool
            .parse(&json!({}))
            .expect("valid environment input");

        smol::block_on(invocation.preflight(&ctx)).expect("environment preflight");
        let result = smol::block_on(invocation.execute(&ctx));
        let output = result.output.expect("successful environment inspection");
        let ToolOutput::Environment {
            headline, facts, ..
        } = &output
        else {
            panic!("expected a structured environment result");
        };

        assert!(!headline.is_empty());
        let workspace = facts
            .iter()
            .find(|fact| fact.label == ENVIRONMENT_WORKSPACE_LABEL)
            .expect("a workspace fact");
        assert!(workspace.value.contains(LOCKFILE));
    }

    /// The reading copy is the card's own rendering, so a session that lists
    /// twenty commands costs the model lines rather than the descriptor's
    /// braces, quoted keys and indentation.
    #[test]
    fn an_environment_result_reads_as_lines_rather_than_as_its_descriptor() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get("execution_environment")
            .expect("registered environment")
            .tool
            .parse(&json!({}))
            .expect("valid environment input");

        smol::block_on(invocation.preflight(&ctx)).expect("environment preflight");
        let result = smol::block_on(invocation.execute(&ctx));
        assert!(result.model_output.is_none(), "the rendering is the text");
        let text = result
            .output
            .expect("successful environment inspection")
            .as_text();

        assert!(serde_json::from_str::<Value>(&text).is_err());
        assert!(text.lines().count() < ENVIRONMENT_MAX_MODEL_LINES);
        assert!(
            text.contains(caudra_agent::ENVIRONMENT_COMMANDS_LABEL),
            "{text}"
        );
    }

    #[test]
    fn patch_preflight_is_non_mutating_and_execution_reuses_prepared_patch() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let entry = registry.get("file_apply_patch").expect("registered patch");
        let invocation = entry
            .tool
            .parse(&json!({"patchText": PATCH}))
            .expect("valid patch");

        let intent = smol::block_on(invocation.preflight(&ctx))
            .expect("patch preflight")
            .expect("permission intent");
        assert!(!root.path().join("created.txt").exists());
        assert_eq!(intent.resources.len(), 1);
        assert_eq!(
            intent.resources[0].access,
            Some(PermissionResourceAccess::Write)
        );
        assert_eq!(
            invocation.mutation_targets(&ctx),
            [root.path().join("created.txt")]
        );

        let result = smol::block_on(invocation.execute(&ctx));
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(root.path().join("created.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            result.written_paths,
            [root.path().join("created.txt").display().to_string()]
        );
        assert!(result.model_suffix.is_none(), "{EXPECT_APPLIED_UNMARKED}");

        let ToolOutput::Patch { files } = result.output.expect("patch output") else {
            panic!("{PATCH_STRUCTURED_MSG}");
        };
        assert_eq!(files.len(), 1, "{PATCH_STRUCTURED_MSG}");
        assert_eq!(files[0].path, "created.txt", "{PATCH_STRUCTURED_MSG}");
        assert_eq!((files[0].additions, files[0].deletions), (1, 0));
    }

    const EDIT_FILE: &str = "editable.txt";
    const EDIT_SEED: &str = "alpha\nbeta\n";
    const EDIT_APPLIED: &str = "alpha\ngamma\n";
    const EDIT_MISS_MSG: &str = "Could not find oldString in the file";
    const PATCH_MISS_MSG: &str = "Failed to find expected lines in";
    const EXPECT_STALE_EDIT_APPLIES: &str =
        "an edit whose oldString still matches applies, however old the model's copy is";
    const EXPECT_STALE_PATCH_APPLIES: &str =
        "a patch whose context Workcell still matches applies, however old the model's copy is";
    const EXPECT_STALE_NOTICE: &str =
        "a failed edit or patch says the file moved, so the retry starts from a re-read";
    const EXPECT_CAUSE_KEPT: &str =
        "the stale notice adds to Workcell's diagnosis, never replaces it";
    const EXPECT_NO_STALE_NOTICE: &str =
        "an unchanged file has no staleness to report, so the failure stands on its own";
    const EXPECT_STALE_WRITE_REFUSED: &str =
        "a blind write cannot detect the conflict itself, so it is still refused up front";

    fn edit_invocation(registry: &ToolRegistry, path: &Path, old: &str) -> Box<dyn ToolInvocation> {
        registry
            .get("file_edit")
            .expect("registered file_edit")
            .tool
            .parse(&json!({ "filePath": path, "oldString": old, "newString": "gamma" }))
            .expect("valid edit input")
    }

    fn update_patch(old: &str) -> String {
        format!("*** Begin Patch\n*** Update File: {EDIT_FILE}\n@@\n-{old}\n+gamma\n*** End Patch")
    }

    fn patch_invocation(registry: &ToolRegistry, patch_text: &str) -> Box<dyn ToolInvocation> {
        registry
            .get("file_apply_patch")
            .expect("registered file_apply_patch")
            .tool
            .parse(&json!({ "patchText": patch_text }))
            .expect("valid patch input")
    }

    fn rendering_invocation(
        registry: &ToolRegistry,
        tool: &str,
        path: &Path,
    ) -> Box<dyn ToolInvocation> {
        let input = match tool {
            "file_write" => json!({ "filePath": path, "content": EDIT_APPLIED }),
            "file_edit" => json!({ "filePath": path, "oldString": "beta", "newString": "gamma" }),
            "file_apply_patch" => json!({ "patchText": update_patch("beta") }),
            _ => json!({ "filePath": path }),
        };
        registry
            .get(tool)
            .unwrap_or_else(|| panic!("registered {tool}"))
            .tool
            .parse(&input)
            .expect("valid input")
    }

    /// Every first-party file tool hands the model a rendering, never the
    /// record its card is drawn from. Serializing the record sent the payload
    /// twice, once escaped inside a JSON string, and made a direct call read
    /// differently from the same call inside a batch, which renders its
    /// children through `as_text`.
    #[test_case("file_read" ; "read")]
    #[test_case("file_write" ; "write")]
    #[test_case("file_edit" ; "edit")]
    #[test_case("file_apply_patch" ; "patch")]
    fn a_file_result_reads_as_its_rendering_rather_than_as_its_record(tool: &str) {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context(root.path(), Arc::clone(&registry), CancelToken::none());
        let invocation = rendering_invocation(&registry, tool, &path);

        smol::block_on(invocation.preflight(&ctx)).expect("preflight");
        let result = smol::block_on(invocation.execute(&ctx));
        let output = result.output.expect("a successful call");
        let batched = output.as_text();
        let direct = result.model_output.unwrap_or_else(|| batched.clone());

        assert_eq!(direct, batched, "{EXPECT_SAME_AS_BATCHED}");
        assert!(
            serde_json::from_str::<Value>(&direct).is_err(),
            "{EXPECT_NO_JSON_FOR_THE_MODEL}: {direct}"
        );
    }

    fn seeded(root: &Path) -> PathBuf {
        let path = root.join(EDIT_FILE);
        std::fs::write(&path, EDIT_SEED).expect("seeding the edited file");
        path
    }

    /// The stale check used to reject the call before Workcell ever looked at
    /// the file, so an edit that would have applied cleanly cost a re-read.
    #[test]
    fn stale_edit_that_still_matches_applies() {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = tracking_context(root.path(), Arc::clone(&registry), &path, true);
        let invocation = edit_invocation(&registry, &path, "beta");
        smol::block_on(invocation.preflight(&ctx)).expect("edit preflight");

        let result = smol::block_on(invocation.execute(&ctx));

        assert!(!result.is_error, "{EXPECT_STALE_EDIT_APPLIES}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("edited file"),
            EDIT_APPLIED
        );
    }

    #[test]
    fn stale_edit_that_fails_names_the_stale_read() {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = tracking_context(root.path(), Arc::clone(&registry), &path, true);
        let invocation = edit_invocation(&registry, &path, "absent");
        smol::block_on(invocation.preflight(&ctx)).expect("edit preflight");

        let error = smol::block_on(invocation.execute(&ctx))
            .output
            .expect_err("a missing oldString fails");

        assert!(
            error.contains(EDIT_MISS_MSG),
            "{EXPECT_CAUSE_KEPT}: {error}"
        );
        assert!(
            error.contains(STALE_READ_MSG),
            "{EXPECT_STALE_NOTICE}: {error}"
        );
    }

    #[test]
    fn fresh_edit_failure_omits_the_stale_notice() {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = tracking_context(root.path(), Arc::clone(&registry), &path, false);
        let invocation = edit_invocation(&registry, &path, "absent");
        smol::block_on(invocation.preflight(&ctx)).expect("edit preflight");

        let error = smol::block_on(invocation.execute(&ctx))
            .output
            .expect_err("a missing oldString fails");

        assert!(
            error.contains(EDIT_MISS_MSG),
            "{EXPECT_CAUSE_KEPT}: {error}"
        );
        assert!(
            !error.contains(STALE_READ_MSG),
            "{EXPECT_NO_STALE_NOTICE}: {error}"
        );
    }

    #[test]
    fn stale_patch_that_still_matches_applies() {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = tracking_context(root.path(), Arc::clone(&registry), &path, true);
        let invocation = patch_invocation(&registry, &update_patch("beta"));
        smol::block_on(invocation.preflight(&ctx)).expect(EXPECT_STALE_PATCH_APPLIES);

        let result = smol::block_on(invocation.execute(&ctx));

        assert!(!result.is_error, "{EXPECT_STALE_PATCH_APPLIES}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("patched file"),
            EDIT_APPLIED
        );
    }

    /// A patch matches its context while planning, so its failure surfaces from
    /// preflight and the notice has to be attached there.
    #[test]
    fn stale_patch_that_fails_names_the_stale_read() {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = tracking_context(root.path(), Arc::clone(&registry), &path, true);
        let invocation = patch_invocation(&registry, &update_patch("absent"));

        let error =
            smol::block_on(invocation.preflight(&ctx)).expect_err("unmatched context fails");

        assert!(
            error.message.contains(PATCH_MISS_MSG),
            "{EXPECT_CAUSE_KEPT}: {error}"
        );
        assert!(
            error.message.contains(STALE_READ_MSG),
            "{EXPECT_STALE_NOTICE}: {error}"
        );
    }

    #[test]
    fn fresh_patch_failure_omits_the_stale_notice() {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = tracking_context(root.path(), Arc::clone(&registry), &path, false);
        let invocation = patch_invocation(&registry, &update_patch("absent"));

        let error =
            smol::block_on(invocation.preflight(&ctx)).expect_err("unmatched context fails");

        assert!(
            error.message.contains(PATCH_MISS_MSG),
            "{EXPECT_CAUSE_KEPT}: {error}"
        );
        assert!(
            !error.message.contains(STALE_READ_MSG),
            "{EXPECT_NO_STALE_NOTICE}: {error}"
        );
    }

    #[test]
    fn stale_write_is_still_refused() {
        let root = TempDir::new().expect("tempdir");
        let path = seeded(root.path());
        let (_host, registry) = host_and_registry(root.path());
        let ctx = tracking_context(root.path(), Arc::clone(&registry), &path, true);
        let invocation = registry
            .get("file_write")
            .expect("registered file_write")
            .tool
            .parse(&json!({ "filePath": &path, "content": "clobbered" }))
            .expect("valid write input");
        smol::block_on(invocation.preflight(&ctx)).expect("write preflight");

        let error = smol::block_on(invocation.execute(&ctx))
            .output
            .expect_err(EXPECT_STALE_WRITE_REFUSED);

        assert!(
            error.contains(STALE_READ_MSG),
            "{EXPECT_STALE_WRITE_REFUSED}: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("untouched file"),
            EDIT_SEED,
            "{EXPECT_STALE_WRITE_REFUSED}"
        );
    }

    #[test_case(json!({"pattern": "needle"}), "needle" ; "a_rootless_search_shows_only_its_pattern")]
    #[test_case(json!({"pattern": "needle", "path": "site/docs"}), "needle in site/docs" ; "a_search_root_joins_the_pattern")]
    #[test_case(json!({"pattern": "needle", "path": "  "}), "needle" ; "a_blank_root_is_not_a_root")]
    fn grep_headers_report_where_the_search_ran(input: serde_json::Value, expected: &str) {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let invocation = registry
            .get("file_grep")
            .expect("registered file_grep")
            .tool
            .parse(&input)
            .expect("valid file_grep input");

        assert_eq!(invocation.start_header().into_ready().text(), expected);
    }

    #[test]
    fn cancelled_shell_sets_error_marker() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let (trigger, token) = CancelToken::new();
        let ctx = context(root.path(), Arc::clone(&registry), token);
        let entry = registry.get("shell").expect("registered shell");
        let invocation = entry
            .tool
            .parse(&json!({"command": "sleep 30"}))
            .expect("valid shell input");
        smol::block_on(invocation.preflight(&ctx)).expect("shell preflight");
        trigger.cancel();

        let result = smol::block_on(invocation.execute(&ctx));
        assert!(result.is_error);
        assert_eq!(result.failure, Some(ToolFailure::Cancelled));
        assert_eq!(result.output.expect_err(SHELL_CANCELLED), SHELL_CANCELLED);
    }

    #[test]
    fn dispatcher_permission_denial_does_not_apply_prepared_patch() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context_with_mode(
            root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
            AgentMode::Build,
            DefaultEffect::Deny,
        );

        let done = smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "patch-denied".into(),
            "file_apply_patch",
            &json!({"patchText": PATCH}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ));

        assert!(done.is_error);
        assert!(!root.path().join("created.txt").exists());
    }

    #[test]
    fn plan_mode_blocks_non_plan_patch_after_preflight() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context_with_mode(
            root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
            AgentMode::Plan(root.path().join("plan.md")),
            DefaultEffect::Allow,
        );

        let done = smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "patch-plan-blocked".into(),
            "file_apply_patch",
            &json!({"patchText": PATCH}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ));

        assert!(done.is_error);
        assert!(!root.path().join("created.txt").exists());
    }

    #[test]
    fn plan_mode_blocks_shell_before_execution() {
        let root = TempDir::new().expect("tempdir");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context_with_mode(
            root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
            AgentMode::Plan(root.path().join("plan.md")),
            DefaultEffect::Allow,
        );

        let done = smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "shell-plan-blocked".into(),
            "shell",
            &json!({"command": "touch should-not-exist"}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ));

        assert!(done.is_error);
        assert!(!root.path().join("should-not-exist").exists());
    }

    #[test]
    fn plan_mode_auto_allows_the_exact_external_plan_file() {
        let root = TempDir::new().expect("tempdir");
        let plans = TempDir::new().expect("plans tempdir");
        let plan = plans.path().join("plan.md");
        let (_host, registry) = host_and_registry(root.path());
        let ctx = context_with_mode(
            root.path(),
            Arc::clone(&registry),
            CancelToken::none(),
            AgentMode::Plan(plan.clone()),
            DefaultEffect::Prompt,
        );

        let done = smol::block_on(caudra_agent::agent::tool_dispatch::run(
            &registry,
            None,
            "plan-write".into(),
            "file_write",
            &json!({"filePath": plan, "content": "approved plan"}),
            &ctx,
            caudra_agent::agent::tool_dispatch::Emit::Silent,
        ));

        assert!(!done.is_error, "{}", done.output.as_text());
        assert!(done.wrote_to(&plan));
        assert_eq!(std::fs::read_to_string(&plan).unwrap(), "approved plan");
    }

    const MISSING_NAME: &str = "does-not-exist.txt";
    const PRESENT_NAME: &str = "present.txt";

    fn preflight_error(root: &Path, tool: &str, input: Value) -> Result<(), ToolError> {
        let (_host, registry) = host_and_registry(root);
        let ctx = context(root, Arc::clone(&registry), CancelToken::none());
        let invocation = registry
            .get(tool)
            .unwrap_or_else(|| panic!("registered {tool}"))
            .tool
            .parse(&input)
            .expect("valid input");
        smol::block_on(invocation.preflight(&ctx)).map(|_| ())
    }

    #[test_case("file_read", json!({"filePath": MISSING_NAME}); "reading a file that is not there")]
    #[test_case("file_index", json!({"path": MISSING_NAME}); "indexing a path that is not there")]
    #[test_case(
        "file_grep",
        json!({"pattern": "needle", "path": MISSING_NAME});
        "searching under a root that is not there"
    )]
    #[test_case(
        "file_glob",
        json!({"pattern": "*.rs", "path": MISSING_NAME});
        "globbing under a root that is not there"
    )]
    /// The property that matters is that an impossible read never costs a
    /// prompt, whichever layer refuses it. `file_index` is already refused inside
    /// Workcell; the rest reach the check here.
    fn a_read_of_a_missing_path_fails_instead_of_asking(tool: &str, input: Value) {
        let root = TempDir::new().expect("tempdir");

        let error = preflight_error(root.path(), tool, input).expect_err("preflight must refuse");

        assert!(
            error.message.contains(MISSING_NAME),
            "the refusal must name the absent path, got {error:?}"
        );
        assert_eq!(error.failure, ToolFailure::NotFound);
    }

    #[test]
    fn the_missing_read_refusal_comes_from_this_crate() {
        let root = TempDir::new().expect("tempdir");

        let error = preflight_error(
            root.path(),
            "file_glob",
            json!({"path": MISSING_NAME, "pattern": "*"}),
        )
        .expect_err("preflight must refuse");

        assert!(
            error.message.contains(MISSING_READ_TARGET),
            "expected our own refusal, got {error:?}"
        );
    }

    /// A write to a path that is not there creates it, which is exactly the call
    /// worth confirming, so it must still reach the permission prompt.
    #[test]
    fn a_write_to_a_missing_path_still_asks() {
        let root = TempDir::new().expect("tempdir");

        let result = preflight_error(
            root.path(),
            "file_write",
            json!({"filePath": MISSING_NAME, "content": "new"}),
        );

        assert!(result.is_ok(), "a write must still prepare: {result:?}");
    }

    #[test_case(PRESENT_NAME; "an existing file still prepares")]
    #[test_case("."; "an existing directory still prepares")]
    fn a_read_of_a_present_path_still_asks(path: &str) {
        let root = TempDir::new().expect("tempdir");
        std::fs::write(root.path().join(PRESENT_NAME), "body").expect("seed file");

        let result = preflight_error(root.path(), "file_read", json!({"filePath": path}));

        assert!(result.is_ok(), "an existing path must prepare: {result:?}");
    }
}
