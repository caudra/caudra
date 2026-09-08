#![forbid(unsafe_code)]

mod read_only_shell;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use caudra_agent::patch;
use caudra_agent::permissions::{
    CONFINED_READ_ATTRIBUTE, CONFINED_READ_VALUE, PermissionAuthorityProfile, PermissionResource,
    PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
    filesystem_permission_resource, shell_permission_scope,
};
use caudra_agent::tools::{
    BoxFuture, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
    PermissionIntent, PermissionScopes, PlanModeAccess, RegistryError, Tool, ToolAudience,
    ToolContext, ToolEffect, ToolExecResult, ToolInvocation, ToolLive, ToolRegistry, ToolSource,
    expand_tilde,
};
use caudra_agent::{
    AgentEvent, CodeGraphRow, CodeGraphSource, GrepFileEntry, GrepMatchGroup, INDEX_TRUNCATED,
    IndexDirectoryEntry as AgentIndexDirectoryEntry,
    IndexDirectoryEntryKind as AgentIndexDirectoryEntryKind, IndexLine as AgentIndexLine,
    IndexLineSemantic as AgentIndexLineSemantic, IndexOutput as AgentIndexOutput,
    IndexSourceRange as AgentIndexSourceRange, PatchedFile, SearchCap,
    ShellFilterInfo as AgentShellFilterInfo, ShellOutput as AgentShellOutput, SnapshotLine,
    TextOutput, ToolInput, ToolOutput,
};
use futures_lite::future;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;
use workcell::ToolSpec;
#[cfg(test)]
use workcell::code::bundled_worker_available;
use workcell::code::{CodeConfiguration, CodeExecution, CodeInput, Outcome, WorkerSource};
use workcell::code_graph::{
    CodeContextInput, CodeExpandInput, CodeGraphLimits, CodeGraphToolGroup, CodeImpactInput,
    CodeMapInput, CodeRefsInput, GraphProgress, GraphProgressSink, ModelText as CodeGraphModelText,
    RankedSymbol, ReachedSymbol, SelectorRefusal, SymbolRef, crawl_filesystem_limits, fit,
};
use workcell::environment::{
    ExecutionEnvironmentError, ExecutionEnvironmentResult, ToolGroupDisclosure,
};
use workcell::files::{
    FileApplyPatchInput, FileApplyPatchOutput, FileDiff, FileEditInput, FileEditOutput,
    FileGlobInput, FileGlobOutput, FileGrepInput, FileGrepOutput, FileReadInput, FileReadOutput,
    FileResource, FileResourceAccess, FileToolGroup, FileWriteInput, FileWriteOutput,
    IndexDirectoryEntryKind, IndexExecutionConfiguration, IndexInput, IndexLimits,
    IndexLineSemantic, IndexOutput as WorkcellIndexOutput, ModelText, PreparedFilePatch,
};
use workcell::output_filter::RowRenderer;
use workcell::shell::{
    PreparedShell, ShellExecution, ShellFilterInfo as WorkcellShellFilterInfo, ShellInput,
    ShellOutput as WorkcellShellOutput, ShellProgressChunk, ShellProgressSink, ShellStream,
    ShellToolGroup,
};
use workcell::web::{
    PreparedWebfetch, PreparedWebsearch, ProxyConfiguration, WebExecution, WebToolGroup,
    WebfetchInput, WebfetchOutput, WebsearchExecutionConfiguration, WebsearchInput,
    WebsearchOutput,
};
use workcell::{CodeToolGroup, ExecutionEnvironment};

pub const OWNER: &str = "workcell";
/// Shared with the tests so a wording change cannot silently pass an assertion.
pub const MISSING_READ_TARGET: &str = "No such file or directory";
pub const NATIVE_TOOL_NAMES: &[&str] = &[
    "file_read",
    "file_glob",
    "file_grep",
    "file_write",
    "file_edit",
    "file_apply_patch",
    "index",
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
const PROGRESS_MAX_BYTES: usize = 64 * 1024;
const PROGRESS_TRUNCATED: &str = "[earlier output truncated]\n";
const BYTES_PER_MIB: usize = 1024 * 1024;
const NORMALIZED_COMMAND_ATTRIBUTE: &str = "normalized_command";
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

struct HostInner {
    runtime: Runtime,
    projects: tokio::sync::Mutex<HashMap<PathBuf, ProjectGroups>>,
    web: WebToolGroup,
    code: Option<Arc<CodeToolGroup>>,
}

impl HostInner {
    async fn project_groups(&self, cwd: PathBuf) -> Result<ProjectGroups, String> {
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let mut projects = self.projects.lock().await;
        if let Some(groups) = projects.get(&cwd) {
            return Ok(groups.clone());
        }
        let files = FileToolGroup::new_unconfined(&cwd, ALLOW_WRITE, None)
            .await
            .map_err(|error| error.to_string())?;
        let shell = ShellToolGroup::new_unconfined(&cwd)
            .await
            .map_err(|error| error.to_string())?;
        let code_graph = code_graph_group(&cwd).await?;
        let environment = Arc::new(ExecutionEnvironment::collect(Some(&cwd)).await);
        let groups = ProjectGroups {
            files,
            shell,
            code_graph,
            environment,
        };
        projects.insert(cwd, groups.clone());
        Ok(groups)
    }

    async fn run<T, F, Fut>(&self, ctx: &ToolContext, operation: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let deadline = ctx.deadline.remaining()?;
        let cancellation = CancellationToken::new();
        if ctx.cancel.is_cancelled() {
            cancellation.cancel();
        }
        let operation_cancellation = cancellation.clone();
        let operation = operation(operation_cancellation.clone());
        let mut task = Box::pin(self.runtime.spawn(async move {
            tokio::pin!(operation);
            let Some(deadline) = deadline else {
                return Ok(operation.await);
            };
            tokio::select! {
                output = &mut operation => Ok(output),
                () = tokio::time::sleep(deadline) => {
                    operation_cancellation.cancel();
                    let _ = operation.await;
                    Err(caudra_agent::tools::DEADLINE_EXCEEDED.to_owned())
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
        result.map_err(|error| format!("Workcell runtime task failed: {error}"))?
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
            let shell = ShellToolGroup::new_unconfined(&project_cwd).await;
            let code_graph = code_graph_group(&project_cwd).await;
            let environment = ExecutionEnvironment::collect(Some(&project_cwd)).await;
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
        let shell = shell.map_err(|error| HostError::Shell(error.to_string()))?;
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
        let mut specs = workcell::files::specs(ALLOW_WRITE);
        let year = jiff::Timestamp::now()
            .strftime("%Y")
            .to_string()
            .parse()
            .unwrap_or(2026);
        specs.extend(workcell::web::specs(
            year,
            &self.inner.web.snapshot().configuration,
        ));
        specs.extend(workcell::shell::specs());
        specs.extend(workcell::code_graph::specs());
        if self.inner.code.is_some() || self.reserve_code || include_unavailable_code {
            specs.extend(workcell::code::specs());
        }
        specs.push(workcell::environment::spec());
        specs
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

#[derive(Clone, Copy)]
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
            "index" => Some(Self::Index),
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
            Self::FileWrite | Self::FileEdit | Self::FileApplyPatch | Self::Shell => {
                ToolAudience::MAIN | ToolAudience::GENERAL_SUB
            }
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

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        reject_unknown_fields(&self.spec, input).map_err(ParseError::custom)?;
        let input = Input::parse(self.kind, input.clone()).map_err(ParseError::custom)?;
        Ok(Box::new(WorkcellInvocation {
            host: Arc::clone(&self.host),
            input,
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
            ToolKind::Index => parse_input("index", input).map(Self::Index),
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
    Index(FileToolGroup, FileResource),
    FilePatch(FileToolGroup, PreparedFilePatch),
    Websearch(PreparedWebsearch),
    Webfetch(PreparedWebfetch),
    Shell(ShellToolGroup, PreparedShell),
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
    prepared: Mutex<Option<PreparedInvocation>>,
}

impl WorkcellInvocation {
    async fn prepare(&self, ctx: &ToolContext) -> Result<PermissionIntent, String> {
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
                let (group, resource) = self
                    .host
                    .run(ctx, move |_| async move {
                        let groups = host.project_groups(cwd).await?;
                        let resource = groups
                            .files
                            .inspect_read(&inspection_input)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok::<_, String>((groups.files, resource))
                    })
                    .await??;
                let mut authorized = input.clone();
                authorized.file_path = resource.path.to_string_lossy().into_owned();
                file_prepared(
                    vec![resource],
                    &project,
                    group,
                    Input::FileRead(authorized),
                    &[],
                )
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
                            .map_err(|e| e.to_string())?;
                        let (group, search_path) =
                            confined_traversal_group(groups.files, &resource).await?;
                        Ok::<_, String>((group, resource, search_path))
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
                            .map_err(|e| e.to_string())?;
                        let (group, search_path) =
                            confined_traversal_group(groups.files, &resource).await?;
                        Ok::<_, String>((group, resource, search_path))
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
                            .map_err(|e| e.to_string())?;
                        Ok::<_, String>((groups.files, resource))
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
                            .map_err(|e| e.to_string())?;
                        Ok::<_, String>((groups.files, resource))
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
                            .map_err(|e| e.to_string())?;
                        Ok::<_, String>((groups.files, patch))
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
                        return Err(with_stale_notice(error, stale_notice(ctx, &targets)));
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
                            .map_err(|error| error.to_string())?;
                        Ok::<_, String>((groups.files, resource))
                    })
                    .await??;
                index_prepared(resource, &project, group)
            }
            Input::Websearch(input) => {
                let prepared = self.host.web.prepare_websearch(input.clone())?;
                let intent = PermissionIntent::new(
                    PermissionScopes::single(prepared.permission_query.clone()),
                    vec![PermissionResource {
                        kind: PermissionResourceKind::Query,
                        value: prepared.permission_query.clone(),
                        access: Some(PermissionResourceAccess::Search),
                        protected: false,
                        requires_prompt: false,
                        attributes: BTreeMap::new(),
                    }],
                    PermissionRisk::Low,
                )
                .with_authority(PermissionAuthorityProfile::Query);
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
                    .map_err(|error| error.to_string())?;
                let intent = PermissionIntent::new(
                    PermissionScopes::single(prepared.permission_url.clone()),
                    vec![PermissionResource {
                        kind: PermissionResourceKind::Url,
                        value: prepared.permission_url.clone(),
                        access: Some(PermissionResourceAccess::Read),
                        protected: false,
                        requires_prompt: false,
                        attributes: BTreeMap::new(),
                    }],
                    PermissionRisk::Medium,
                )
                .with_authority(PermissionAuthorityProfile::Url);
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
                        let prepared = group.prepare(input).await?;
                        Ok::<_, String>((group, prepared))
                    })
                    .await??;
                shell_prepared(group, shell, &project)
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
                        Ok::<_, String>(host.project_groups(project).await?.environment)
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
            return Err(format!("{MISSING_READ_TARGET}: {missing}"));
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
    ) -> Result<Arc<CodeGraphToolGroup>, String> {
        let host = Arc::clone(&self.host);
        self.host
            .run(ctx, move |_| async move {
                Ok::<_, String>(host.project_groups(project).await?.code_graph)
            })
            .await?
    }

    async fn take_prepared(&self, ctx: &ToolContext) -> Result<PreparedInvocation, String> {
        self.prepare(ctx).await?;
        self.prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| "Workcell invocation preparation was already consumed".into())
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

impl ToolInvocation for WorkcellInvocation {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(match &self.input {
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
        }))
    }

    /// A patch is deliberately absent: its result is the same diff, rendered
    /// with real line numbers, so echoing the request above it says
    /// everything twice and truncates both halves.
    fn start_input(&self) -> Option<ToolInput> {
        match &self.input {
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

    fn mutation_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
        self.prepared_targets(|prepared| &prepared.mutation_targets)
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
            Some(PreparedExecution::Shell(_, shell)) => {
                let opaque =
                    shell.analysis().opaque || shell_command_hides_operands(shell.command());
                if read_only_shell::is_read_only(shell.analysis(), opaque) {
                    PlanModeAccess::ReadOnly
                } else {
                    PlanModeAccess::Prompted
                }
            }
            _ => PlanModeAccess::Refused,
        }
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, String>> {
        Box::pin(async move { self.prepare(ctx).await.map(Some) })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let prepared = match self.take_prepared(ctx).await {
                Ok(prepared) => prepared,
                Err(error) => return Err(error).into(),
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
            (Input::FileRead(_), PreparedExecution::File(group, Input::FileRead(input))) => {
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group.file_read(input, &token).await
                    })
                    .await
                {
                    Ok(Ok(output)) => {
                        if let FileReadOutput::File { path, .. } = &output {
                            ctx.file_tracker.record_read(Path::new(path));
                        }
                        file_read_result(output)
                    }
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(with_stale_notice(error.to_string(), stale)).into(),
                    Err(error) => Err(error).into(),
                }
            }
            // No stale check: the patch only reaches execution once Workcell has
            // matched every context line at plan time, so a stale copy of the
            // file has already failed in `prepare`.
            (Input::FileApplyPatch(_), PreparedExecution::FilePatch(group, patch)) => {
                let result = self
                    .host
                    .run(ctx, move |token| async move {
                        group.execute_prepared_patch(patch, &token).await
                    })
                    .await
                    .and_then(|result| result.map_err(|error| error.to_string()));
                match result {
                    Ok(output) => {
                        if output.applied {
                            for path in applied_patch_paths(&output) {
                                ctx.file_tracker.record_read(Path::new(&path));
                            }
                        }
                        file_patch_result(output)
                    }
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
                }
            }
            (Input::Websearch(_), PreparedExecution::Websearch(prepared)) => {
                match self
                    .host
                    .run(ctx, {
                        let web = self.host.web.clone();
                        move |token| async move { web.execute_websearch(prepared, token).await }
                    })
                    .await
                {
                    Ok(Ok(execution)) => websearch_result(execution),
                    Ok(Err(error)) => Err(error).into(),
                    Err(error) => Err(error).into(),
                }
            }
            (Input::Webfetch(_), PreparedExecution::Webfetch(prepared)) => {
                match self
                    .host
                    .run(ctx, {
                        let web = self.host.web.clone();
                        move |token| async move { web.execute_webfetch(prepared, token).await }
                    })
                    .await
                {
                    Ok(Ok(execution)) => webfetch_result(execution),
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
                }
            }
            (Input::Shell(_), PreparedExecution::Shell(group, prepared)) => {
                let progress = Arc::new(NativeProgressSink::new(ctx));
                progress.publish_live_buf(ctx);
                match self
                    .host
                    .run(ctx, move |token| async move {
                        group
                            .execute_prepared(prepared, token, Some(progress))
                            .await
                    })
                    .await
                {
                    Ok(Ok(Some(execution))) => shell_result(execution),
                    Ok(Ok(None)) => Err("Shell execution cancelled".into()).into(),
                    Ok(Err(error)) => Err(error).into(),
                    Err(error) => Err(error).into(),
                }
            }
            (Input::Code(input), PreparedExecution::None) => {
                let Some(code) = self.host.code.clone() else {
                    return Err(CODE_WORKER_UNAVAILABLE.into()).into();
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
                    Ok(Ok(None)) => Err("Code execution cancelled".into()).into(),
                    Ok(Err(error)) => Err(error).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(error.to_string()).into(),
                    Err(error) => Err(error).into(),
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
                    Ok(Err(error)) => Err(environment_error(error)).into(),
                    Err(error) => Err(error).into(),
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

async fn confined_traversal_group(
    unconfined: FileToolGroup,
    resource: &FileResource,
) -> Result<(FileToolGroup, String), String> {
    let is_directory = tokio::fs::metadata(&resource.path)
        .await
        .is_ok_and(|metadata| metadata.is_dir());
    if !is_directory {
        return Ok((unconfined, resource.path.to_string_lossy().into_owned()));
    }
    let limits = *unconfined.limits();
    let confined = FileToolGroup::new(&resource.path, false, Some(limits))
        .await
        .map_err(|error| error.to_string())?;
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
            Some(PermissionResourceAccess::Read) | Some(PermissionResourceAccess::Search)
        );
        let filesystem = matches!(
            resource.kind,
            PermissionResourceKind::File | PermissionResourceKind::Directory
        );
        (reads && filesystem && !Path::new(&resource.value).exists())
            .then_some(resource.value.as_str())
    })
}

fn file_prepared(
    resources: Vec<FileResource>,
    project: &Path,
    group: FileToolGroup,
    input: Input,
    input_pointers: &[&str],
) -> PreparedInvocation {
    let permissions = file_permissions(&resources, project);
    let mutation = !permissions.mutation_targets.is_empty();
    PreparedInvocation {
        intent: PermissionIntent::new(
            PermissionScopes {
                scopes: permissions.scopes,
                force_prompt: false,
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
) -> PreparedInvocation {
    let opaque = shell.analysis().opaque || shell_command_hides_operands(shell.command());
    let confined_read = read_only_shell::is_read_only(shell.analysis(), opaque)
        && read_only_shell::stays_in_project(shell.analysis(), shell.workdir(), project);
    let workdir = shell.workdir().to_string_lossy().into_owned();
    let scopes = if opaque || shell.analysis().scopes.is_empty() {
        vec![shell_permission_scope(shell.command(), shell.workdir())]
    } else {
        shell
            .analysis()
            .scopes
            .iter()
            .map(|scope| shell_permission_scope(&scope.source, shell.workdir()))
            .collect()
    };
    let commands: Vec<(String, Option<String>)> = if opaque || shell.analysis().scopes.is_empty() {
        vec![(shell.command().into(), None)]
    } else {
        shell
            .analysis()
            .scopes
            .iter()
            .map(|scope| (scope.source.clone(), Some(scope.normalized.clone())))
            .collect()
    };
    let resources = commands
        .into_iter()
        .map(|(command, normalized)| {
            let mut attributes = BTreeMap::from([("workdir".into(), workdir.clone())]);
            if let Some(normalized) = normalized {
                attributes.insert(NORMALIZED_COMMAND_ATTRIBUTE.into(), normalized);
            }
            if confined_read {
                attributes.insert(
                    CONFINED_READ_ATTRIBUTE.into(),
                    CONFINED_READ_VALUE.to_owned(),
                );
            }
            PermissionResource {
                kind: PermissionResourceKind::Command,
                value: command,
                access: Some(PermissionResourceAccess::Execute),
                protected: opaque,
                requires_prompt: opaque,
                attributes,
            }
        })
        .collect();
    PreparedInvocation {
        intent: PermissionIntent::new(
            // Opaque commands carry `requires_prompt` instead of forcing a prompt on
            // the whole request: scope allows and configured command allows still
            // cannot cover them, while an explicitly confirmed structured authority
            // can.
            PermissionScopes {
                scopes,
                force_prompt: false,
            },
            resources,
            if opaque {
                PermissionRisk::Critical
            } else {
                PermissionRisk::High
            },
        )
        .with_authority(PermissionAuthorityProfile::Shell),
        execution: PreparedExecution::Shell(group, shell),
        // A command's writes are not knowable from its text, so shell neither
        // takes guards nor invalidates the tracker.
        mutation_targets: Vec::new(),
        read_targets: Vec::new(),
    }
}

/// Reports whether a command carries operands that the analyzed scopes drop.
///
/// Shell analysis strips redirection nodes from each scope's source, so a file
/// redirect, heredoc, or here-string would leave the reviewed text describing
/// less than the command actually does. File descriptor duplication such as
/// `2>&1` names no operand and stays reviewable.
fn shell_command_hides_operands(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut index = 0;
    let mut comment_eligible = true;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' => {
                index += 2;
                comment_eligible = false;
            }
            b'$' if bytes.get(index + 1) == Some(&b'\'') => {
                index = skip_quoted(bytes, index + 2, b'\'', true);
                comment_eligible = false;
            }
            b'\'' => {
                index = skip_quoted(bytes, index + 1, b'\'', false);
                comment_eligible = false;
            }
            b'"' => {
                index = skip_quoted(bytes, index + 1, b'"', true);
                comment_eligible = false;
            }
            b'#' if comment_eligible => {
                index = bytes[index..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |offset| index + offset);
            }
            b'<' | b'>' => {
                if !duplicates_descriptor(bytes, index) {
                    return true;
                }
                index += 2;
                comment_eligible = false;
            }
            b';' | b'|' | b'&' | b'(' | b')' => {
                index += 1;
                comment_eligible = true;
            }
            _ => {
                comment_eligible = byte.is_ascii_whitespace();
                index += 1;
            }
        }
    }
    false
}

/// Advances past a quoted span, optionally honoring backslash escapes.
fn skip_quoted(bytes: &[u8], mut index: usize, terminator: u8, escapes: bool) -> usize {
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' if escapes => index += 2,
            byte if byte == terminator => return index + 1,
            _ => index += 1,
        }
    }
    index
}

/// Reports whether the redirect at `index` targets a descriptor rather than a file.
///
/// `>&` and `<&` duplicate or close a descriptor when followed by a digit run or
/// `-`; any other word is a file target that redirects both streams.
fn duplicates_descriptor(bytes: &[u8], index: usize) -> bool {
    if bytes.get(index + 1) != Some(&b'&') {
        return false;
    }
    let mut cursor = index + 2;
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    let digits = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == digits && bytes.get(cursor) != Some(&b'-') {
        return false;
    }
    if bytes.get(cursor) == Some(&b'-') {
        cursor += 1;
    }
    bytes.get(cursor).is_none_or(|byte| {
        byte.is_ascii_whitespace() || matches!(byte, b';' | b'|' | b'&' | b'(' | b')' | b'<' | b'>')
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

fn model_text(output: &impl Serialize) -> String {
    serde_json::to_string_pretty(output).expect("Workcell structured output serializes")
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

fn markdown_code(language: &str, text: &str) -> String {
    format!("```{language}\n{}\n```", text.trim_end())
}

fn file_read_result(output: FileReadOutput) -> ToolExecResult {
    let exact = model_text(&output);
    let state = serde_json::to_value(&output).expect("file read output serializes");
    let tool_output = match output {
        FileReadOutput::Directory { entries, .. } => {
            ToolOutput::ReadDir(text_output(entries.join("\n"), state))
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
    ToolExecResult::from(Ok::<_, String>(tool_output)).with_model_output(Some(exact))
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

fn file_write_result(output: FileWriteOutput, content: String) -> ToolExecResult {
    let exact = model_text(&output);
    let written = output.applied.then(|| output.path.clone());
    let result = if output.applied {
        ToolExecResult::from(Ok::<_, String>(ToolOutput::WriteCode {
            path: output.path.clone(),
            byte_count: content.len(),
            lines: content.lines().map(str::to_owned).collect(),
        }))
        .with_model_output(Some(exact))
    } else {
        // Nothing was written, so there is no content to show. The diff is
        // the whole report, and it reads as one.
        ToolExecResult::from(Ok::<_, String>(ToolOutput::Patch {
            files: vec![patched_file(&output.diff)],
        }))
        .with_model_output(Some(exact))
    };
    result.with_written_paths(written.into_iter().collect())
}

fn file_edit_result(
    output: FileEditOutput,
    old_string: String,
    new_string: String,
    replace_all: bool,
) -> ToolExecResult {
    let exact = model_text(&output);
    let written = output.applied.then(|| output.path.clone());
    let result = if replace_all {
        // Every match moved at once, so there is no single before/after pair
        // to diff. The patch carries all of them with their real line numbers.
        ToolExecResult::from(Ok::<_, String>(ToolOutput::Patch {
            files: vec![patched_file(&output.diff)],
        }))
        .with_model_output(Some(exact))
    } else {
        ToolExecResult::from(Ok::<_, String>(ToolOutput::Diff {
            path: output.path.clone(),
            before: old_string,
            after: new_string,
            summary: output.diff.patch.clone(),
        }))
        .with_model_output(Some(exact))
    };
    result.with_written_paths(written.into_iter().collect())
}

fn patched_file(diff: &FileDiff) -> PatchedFile {
    PatchedFile {
        path: diff.relative_path.clone(),
        patch: diff.patch.clone(),
        additions: diff.additions,
        deletions: diff.deletions,
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
    let exact = model_text(&output);
    let written = applied_patch_paths(&output);
    let files = output
        .files
        .iter()
        .map(|file| PatchedFile {
            path: file.relative_path.clone(),
            patch: file.patch.clone(),
            additions: file.additions,
            deletions: file.deletions,
        })
        .collect();
    ToolExecResult::from(Ok::<_, String>(ToolOutput::Patch { files }))
        .with_model_output(Some(exact))
        .with_written_paths(written)
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
    let is_error = output.exit_code != Some(0) || output.timed_out || output.output_limit_exceeded;
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
    ToolExecResult::from(Ok::<_, String>(ToolOutput::Shell(output)))
        .with_model_output(Some(model_text))
        .with_error(is_error)
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
    let is_error = execution.output.outcome != Outcome::Completed;
    text_result(
        &execution.output,
        execution.model_text.clone(),
        false,
        execution.model_text,
    )
    .with_error(is_error)
}

fn environment_result(result: ExecutionEnvironmentResult) -> ToolExecResult {
    let display = markdown_code("json", &result.model_text);
    text_result(&result.output, display, true, result.model_text)
}

fn environment_error(error: ExecutionEnvironmentError) -> String {
    error.to_string()
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

    fn publish_live_buf(&self, _ctx: &ToolContext) {
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
    use caudra_agent::cancel::CancelToken;
    use caudra_agent::permissions::{
        PermissionManager, PermissionResourceAccess, PermissionResourceKind,
    };
    use caudra_agent::tools::{FileReadTracker, STALE_READ_MSG, interpreter_ctx};
    use caudra_agent::{AgentMode, Envelope, EventSender};
    use caudra_config::{DefaultEffect, Effect, PermissionRule, PermissionsConfig, ToolKey};
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;
    use test_case::test_case;

    const PATCH: &str = "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** End Patch";
    const PREVIEW_FLAG_MSG: &str = "a dry-run argument must fail the call rather than write";
    const PATCH_STRUCTURED_MSG: &str = "a patch reports the files it changed, not a diff blob";
    const FILTERABLE_MAKEFILE: &str = "all:\n\t@echo \"make[1]: Entering directory '/x'\"\n\t@echo \"real build line\"\n\t@echo \"make[1]: Leaving directory '/x'\"\n";

    fn context(root: &Path, registry: Arc<ToolRegistry>, cancel: CancelToken) -> ToolContext {
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
        ctx
    }

    fn host_and_registry(root: &Path) -> (WorkcellHost, Arc<ToolRegistry>) {
        let host = WorkcellHost::new(root, None).expect("Workcell host");
        let registry = Arc::new(ToolRegistry::new());
        host.register(&registry).expect("Workcell registration");
        (host, registry)
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
    #[test_case("cat <<EOF\nvalue\nEOF", true; "heredoc")]
    #[test_case("cat <<<value", true; "here_string")]
    #[test_case("git status # '\n> victim", true; "quote_in_comment_before_redirect")]
    #[test_case("printf foo#bar > output", true; "hash_inside_word_before_redirect")]
    #[test_case(r"printf $'a\'b' > output", true; "redirect_after_ansi_c_quote")]
    #[test_case("cargo check 2>&1 | head -40", false; "stderr_to_stdout")]
    #[test_case("cargo check >&2", false; "stdout_to_stderr")]
    #[test_case("cargo check 1>&2", false; "explicit_stdout_to_stderr")]
    #[test_case("exec 3<&0", false; "input_descriptor_duplicate")]
    #[test_case("cargo check >& 2", false; "spaced_descriptor_duplicate")]
    #[test_case("cargo check 2>&-", false; "descriptor_close")]
    #[test_case("printf ok # > ignored", false; "redirect_inside_comment")]
    #[test_case("printf '%s > %s' left right", false; "single_quoted_literal")]
    #[test_case(r#"printf ">""#, false; "double_quoted_literal")]
    #[test_case(r"printf \>", false; "escaped_literal")]
    #[test_case(r"printf $'a\'b'", false; "ansi_c_quoted_literal")]
    #[test_case(r"printf $'>'", false; "ansi_c_quoted_redirect_literal")]
    fn shell_commands_hiding_operands_require_exact_authority(command: &str, expected: bool) {
        assert_eq!(shell_command_hides_operands(command), expected);
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
        assert_eq!(intent.resources.len(), 1);
        assert_eq!(intent.resources[0].value, command);
        assert!(intent.resources[0].protected);
        assert!(intent.resources[0].requires_prompt);
    }

    /// The classifier's answer has to reach the permission layer or it only ever
    /// gated plan mode. This attribute is what the builtin allow rule keys on,
    /// so marking a line is the whole difference between running and asking.
    #[test_case("git status --short" => true ; "a read that cannot leave the project")]
    #[test_case("cat Cargo.toml" => true ; "a read of a relative path")]
    #[test_case("cat /etc/shadow" => false ; "a read that leaves the project")]
    #[test_case("cat ../../secret" => false ; "a read that climbs out of it")]
    #[test_case("rm -rf build" => false ; "not a read at all")]
    #[test_case("git status > out.txt" => false ; "an opaque line is never marked")]
    fn shell_preflight_marks_only_a_confined_read(command: &str) -> bool {
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

        !intent.resources.is_empty()
            && intent.resources.iter().all(|resource| {
                resource
                    .attributes
                    .get(CONFINED_READ_ATTRIBUTE)
                    .map(String::as_str)
                    == Some(CONFINED_READ_VALUE)
            })
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
            .find(|spec| spec.name == "index")
            .expect("index spec");
        let registered = registry.get("index").expect("registered index");

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
            ToolAudience::WORKFLOW,
        ] {
            let definitions = registry.definitions(
                &caudra_agent::template::Vars::new(),
                &DescriptionContext {
                    filter: &caudra_agent::tools::ToolFilter::All,
                    audience,
                    workflow: audience == ToolAudience::WORKFLOW,
                },
                false,
            );
            assert!(
                definitions
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|definition| definition["name"] == "index"),
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
                .filter(|entry| entry.name() == "index")
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
                .get("index")
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
            .get("index")
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
            .get("index")
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
            .get("index")
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
            .get("index")
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
            ("index", ToolEffect::ReadOnly),
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
    /// plan run writes silently. Building runs it; planning forces the prompt,
    /// and with no responder that means denied.
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
        let result = file_grep_result(FileGrepOutput {
            cwd: "/project".into(),
            relative_path: ".".into(),
            pattern: "needle".into(),
            include: None,
            rows: vec![workcell::files::FileGrepRow {
                path: "/project/a.rs".into(),
                relative_path: "a.rs".into(),
                line: 3,
                text: "needle".into(),
            }],
            matches: 1,
            files_scanned: 40,
            files_listed: 900,
            truncated: true,
        });
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

        let grep = file_grep_result(FileGrepOutput {
            cwd: "/project".into(),
            relative_path: ".".into(),
            pattern: "main".into(),
            include: Some("*.rs".into()),
            rows: vec![workcell::files::FileGrepRow {
                path: "/project/src/lib.rs".into(),
                relative_path: "src/lib.rs".into(),
                line: 1,
                text: "fn main() {}".into(),
            }],
            matches: 1,
            files_scanned: 1,
            files_listed: 1,
            truncated: false,
        })
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
                existed: true,
                applied: true,
                diff: diff.clone(),
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

    /// `file_read` and `index` record the file's mtime only after reading its
    /// content. Without a shared guard a write landing in between records an
    /// mtime newer than what the model saw, and the next edit passes its stale
    /// check holding stale content.
    #[test_case("file_read", json!({ "filePath": CONTENT_FILE }), true ; "file_read_guards_its_file")]
    #[test_case("index", json!({ "path": CONTENT_FILE }), true ; "index_guards_its_file")]
    #[test_case("index", json!({ "path": "." }), false ; "index_does_not_guard_a_directory")]
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
        ctx.deadline = caudra_agent::tools::Deadline::after(std::time::Duration::ZERO);
        let invocation = registry
            .get("file_read")
            .expect("registered file read")
            .tool
            .parse(&json!({"filePath": "missing.txt"}))
            .expect("valid read input");

        assert_eq!(
            smol::block_on(invocation.preflight(&ctx)).unwrap_err(),
            caudra_agent::tools::DEADLINE_EXCEEDED
        );
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
        std::fs::write(session_root.path().join("package-lock.json"), "{}").unwrap();
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
        let ToolOutput::Markdown(text) = &output else {
            panic!("expected highlighted JSON environment output");
        };
        assert!(text.text.starts_with("```json\n{"));
        assert!(text.text.ends_with("\n```"));
        let lockfiles = output.state().expect("structured environment")["workspace"]
            ["packageManager"]["lockfiles"]
            .as_array()
            .expect("lockfiles");

        assert!(lockfiles.iter().any(|name| name == "package-lock.json"));
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
        let model_output: Value = serde_json::from_str(
            result
                .model_output
                .as_deref()
                .expect("exact Workcell model output"),
        )
        .expect("structured model output");
        assert_eq!(model_output["applied"], true);

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
            error.contains(PATCH_MISS_MSG),
            "{EXPECT_CAUSE_KEPT}: {error}"
        );
        assert!(
            error.contains(STALE_READ_MSG),
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
            error.contains(PATCH_MISS_MSG),
            "{EXPECT_CAUSE_KEPT}: {error}"
        );
        assert!(
            !error.contains(STALE_READ_MSG),
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
        assert!(result.output.is_err());
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
        assert_eq!(std::fs::read_to_string(plan).unwrap(), "approved plan");
    }

    const MISSING_NAME: &str = "does-not-exist.txt";
    const PRESENT_NAME: &str = "present.txt";

    fn preflight_error(root: &Path, tool: &str, input: Value) -> Result<(), String> {
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
    #[test_case("index", json!({"path": MISSING_NAME}); "indexing a path that is not there")]
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
    /// prompt, whichever layer refuses it. `index` is already refused inside
    /// Workcell; the rest reach the check here.
    fn a_read_of_a_missing_path_fails_instead_of_asking(tool: &str, input: Value) {
        let root = TempDir::new().expect("tempdir");

        let error = preflight_error(root.path(), tool, input).expect_err("preflight must refuse");

        assert!(
            error.contains(MISSING_NAME),
            "the refusal must name the absent path, got {error:?}"
        );
    }

    #[test]
    fn the_missing_read_refusal_comes_from_this_crate() {
        let root = TempDir::new().expect("tempdir");

        let error = preflight_error(root.path(), "file_read", json!({"filePath": MISSING_NAME}))
            .expect_err("preflight must refuse");

        assert!(
            error.contains(MISSING_READ_TARGET),
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
