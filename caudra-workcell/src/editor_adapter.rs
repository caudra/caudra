use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

use caudra_agent::AgentMode;
use caudra_agent::permissions::editor::{
    ArgumentMode, AuthorityCatalog, EditableAuthorityDescriptor, PermissionAuthorityLease,
    PermissionAuthorityProvider, PermissionEditError, PermissionExampleAnalysis,
    ResourceCapability, SelectorMode, TemplateAnalysis, TemplateSource,
};
use caudra_agent::permissions::{
    PermissionAuthorityProfile, PermissionCapabilityFamily, PermissionResource,
    PermissionResourceAccess, PermissionResourceKind, PermissionRisk, PermissionSubject,
    pattern_recognition::ObservationProvenance,
};
use caudra_agent::tools::registry::{RegistryAuthoritySnapshot, TrustedToolSource};
use caudra_agent::tools::{
    PermissionIntent, PermissionScopes, PlanModeAccess, ToolAudience, ToolFilter, ToolRegistry,
    expand_tilde,
};
use caudra_config::{ShellNativeRedirect, ToolKey};
use caudra_storage::permission_state::BROWSE_RECURSION_ATTRIBUTE;
use caudra_workspace::WorkspaceSession;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use workcell::{
    files::{FileReadInput, FileResourceAccess, FileToolGroup},
    shell::PreparedShell,
};

use crate::{
    HostInner, Input, OWNER, PreparedExecution, PreparedInvocation, ToolKind, WorkcellHost,
    code_graph_prepared, confined_traversal_group, exact_custom_prepared, file_patch_prepared,
    file_prepared, file_read_prepared, index_prepared, missing_read_target, pattern_analysis,
    read_only_shell, reject_unknown_fields, shell_prepared,
};

const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EXAMPLE_BYTES: usize = 256 * 1024;
const DISABLED: &str = "This tool is disabled for the current agent";
const READ_ONLY: &str = "This authority is unavailable in strict read-only mode";
const UNSUPPORTED: &str = "This registration has no audited host editing contract";
const REMOTE_UNSUPPORTED: &str =
    "Remote authority editing requires a nonexecuting server analysis and binding lease";
const PLAN_RESTRICTED: &str = "This example is outside the current plan's execution boundary";
const NO_HOST: &str = "The local Workcell host is unavailable";
const NO_TEMPLATE: &str = "Fresh analysis requires one complete, static, reviewable shell command";
const WORKDIR_ATTRIBUTE: &str = "workdir";
const LOCAL_IN_REMOTE: &str = "The local Workcell host is not the active remote workspace";

#[derive(Clone)]
pub struct PermissionEditorRuntime {
    pub project: PathBuf,
    pub mode: AgentMode,
    pub tool_filter: ToolFilter,
    pub audience: ToolAudience,
    pub workspace: Option<WorkspaceSession>,
}

pub struct PermissionEditorContext {
    current: RwLock<(u64, PermissionEditorRuntime)>,
}

impl PermissionEditorContext {
    pub fn new(runtime: PermissionEditorRuntime) -> Result<Self, PermissionEditError> {
        validate_runtime(&runtime)?;
        Ok(Self {
            current: RwLock::new((0, runtime)),
        })
    }

    pub fn replace(&self, runtime: PermissionEditorRuntime) -> Result<(), PermissionEditError> {
        validate_runtime(&runtime)?;
        let mut current = self
            .current
            .write()
            .map_err(|_| unavailable("Editor context lock is poisoned"))?;
        let revision = current
            .0
            .checked_add(1)
            .ok_or_else(|| unavailable("Editor context revision exhausted"))?;
        *current = (revision, runtime);
        Ok(())
    }
}

pub fn permission_authority_provider(
    registry: Arc<ToolRegistry>,
    context: Arc<PermissionEditorContext>,
    host: Option<&WorkcellHost>,
) -> Arc<dyn PermissionAuthorityProvider> {
    Arc::new(WorkcellAuthorityProvider {
        registry,
        context,
        host: host.map(|host| Arc::clone(&host.inner)),
    })
}

struct WorkcellAuthorityProvider {
    registry: Arc<ToolRegistry>,
    context: Arc<PermissionEditorContext>,
    host: Option<Arc<HostInner>>,
}

struct WorkcellAuthorityLease<'a> {
    _registry: RegistryAuthoritySnapshot<'a>,
    runtime: RwLockReadGuard<'a, (u64, PermissionEditorRuntime)>,
    host: Option<&'a Arc<HostInner>>,
    catalog: AuthorityCatalog,
}

impl PermissionAuthorityProvider for WorkcellAuthorityProvider {
    fn acquire(
        &self,
        project: &Path,
    ) -> Result<Box<dyn PermissionAuthorityLease + '_>, PermissionEditError> {
        let runtime = self
            .context
            .current
            .read()
            .map_err(|_| unavailable("Editor context lock is poisoned"))?;
        if runtime.1.project != project {
            return Err(PermissionEditError::Conflict);
        }
        let registry = self.registry.authority_snapshot();
        let specs = self
            .host
            .as_ref()
            .map(|host| host.specs(true))
            .unwrap_or_default();
        let authorities = registry
            .tools()
            .iter()
            .filter_map(|entry| {
                let source = TrustedToolSource::from_registered(entry, None)?;
                let spec = local_contract(&source)
                    .and_then(|contract| specs.iter().find(|spec| spec.contract_id == contract));
                let kind = spec.and_then(|spec| ToolKind::from_name(spec.name));
                let mut authority = descriptor(entry.name(), source, kind);
                authority.unavailable = if !runtime.1.tool_filter.matches(entry.name())
                    || kind.is_some_and(|kind| !kind.audience().contains(runtime.1.audience))
                {
                    Some(DISABLED.into())
                } else if (runtime.1.mode.is_read_only() || runtime.1.tool_filter.is_read_only())
                    && !entry.is_safe_in_read_only()
                {
                    Some(READ_ONLY.into())
                } else if matches!(
                    authority.source.subject(),
                    PermissionSubject::RemoteWorkcell { .. }
                        | PermissionSubject::RemoteNative { .. }
                ) {
                    Some(REMOTE_UNSUPPORTED.into())
                } else if runtime.1.workspace.is_some()
                    && local_contract(&authority.source).is_some()
                {
                    Some(LOCAL_IN_REMOTE.into())
                } else if self.host.is_none() && local_contract(&authority.source).is_some() {
                    Some(NO_HOST.into())
                } else if kind.is_none() {
                    Some(UNSUPPORTED.into())
                } else if kind == Some(ToolKind::Code)
                    && self.host.as_ref().is_none_or(|host| host.code.is_none())
                {
                    Some(crate::CODE_WORKER_UNAVAILABLE.into())
                } else {
                    None
                };
                Some(authority)
            })
            .collect();
        let catalog = AuthorityCatalog {
            revision: format!("{}:{}", registry.revision(), runtime.0),
            authorities,
        };
        Ok(Box::new(WorkcellAuthorityLease {
            _registry: registry,
            runtime,
            host: self.host.as_ref(),
            catalog,
        }))
    }
}

impl PermissionAuthorityLease for WorkcellAuthorityLease<'_> {
    fn catalog(&self) -> &AuthorityCatalog {
        &self.catalog
    }

    fn analyze_template(
        &self,
        authority: &EditableAuthorityDescriptor,
        source: &TemplateSource,
    ) -> Result<TemplateAnalysis, PermissionEditError> {
        let workdir = source
            .workdir
            .to_str()
            .ok_or_else(|| unavailable(NO_TEMPLATE))?;
        let input = json!({"command": source.command, "workdir": workdir});
        let prepared = self.prepare(authority, &input)?;
        let PreparedExecution::Shell(_, shell) = prepared.execution else {
            return Err(unavailable(NO_TEMPLATE));
        };
        let program = shell.bash_program().map_err(|_| unavailable(NO_TEMPLATE))?;
        let contexts = shell
            .bash_command_contexts()
            .map_err(|_| unavailable(NO_TEMPLATE))?;
        let facts = pattern_analysis::shell_facts(program, &contexts);
        if facts.opaque || facts.commands.len() != 1 || !source.workdir.is_absolute() {
            return Err(unavailable(NO_TEMPLATE));
        }
        let observation = pattern_analysis::command_observation(
            program,
            &facts.commands[0],
            shell.workdir(),
            &self.runtime.1.project,
            ObservationProvenance::Native,
        )
        .ok_or_else(|| unavailable(NO_TEMPLATE))?;
        Ok(TemplateAnalysis {
            context: observation.context,
            argv: observation.argv,
            roles: observation.roles,
            option_like_data: BTreeSet::new(),
        })
    }

    fn analyze_example(
        &self,
        authority: &EditableAuthorityDescriptor,
        input: &Value,
    ) -> Result<PermissionExampleAnalysis, PermissionEditError> {
        let mut prepared = self.prepare(authority, input)?;
        let runtime = &self.runtime.1;
        if runtime.mode.is_planning() {
            if let PreparedExecution::Shell(_, shell) = &prepared.execution {
                match shell_plan_access(shell) {
                    PlanModeAccess::ReadOnly => {}
                    PlanModeAccess::Prompted => prepared.intent.scopes.plan_scoped = true,
                    PlanModeAccess::Refused | PlanModeAccess::Standard => {
                        return Err(unavailable(PLAN_RESTRICTED));
                    }
                }
            } else if !authority.source.effect().is_safe_in_read_only()
                && (prepared.mutation_targets.is_empty()
                    || prepared
                        .mutation_targets
                        .iter()
                        .any(|target| Some(target.as_path()) != runtime.mode.plan_path()))
            {
                return Err(unavailable(PLAN_RESTRICTED));
            }
            if matches!(runtime.mode, AgentMode::RemotePlan(_))
                && !authority.source.effect().is_safe_in_read_only()
            {
                return Err(unavailable(PLAN_RESTRICTED));
            }
        }
        Ok(PermissionExampleAnalysis {
            tool: ToolKey::native(&authority.key),
            intent: prepared.intent,
            plan_path: runtime.mode.plan_path().map(Path::to_path_buf),
        })
    }
}

impl WorkcellAuthorityLease<'_> {
    fn prepare(
        &self,
        authority: &EditableAuthorityDescriptor,
        raw_input: &Value,
    ) -> Result<PreparedInvocation, PermissionEditError> {
        if !self.catalog.authorities.contains(authority) {
            return Err(PermissionEditError::Conflict);
        }
        if let Some(reason) = &authority.unavailable {
            return Err(unavailable(reason));
        }
        if serde_json::to_vec(raw_input).map_or(true, |input| input.len() > MAX_EXAMPLE_BYTES) {
            return Err(unavailable("Example exceeds the analysis bound"));
        }
        let host = Arc::clone(self.host.ok_or_else(|| unavailable(NO_HOST))?);
        let contract = local_contract(&authority.source).ok_or_else(|| unavailable(UNSUPPORTED))?;
        let spec = host
            .specs(true)
            .into_iter()
            .find(|spec| spec.contract_id == contract)
            .ok_or_else(|| unavailable(UNSUPPORTED))?;
        reject_unknown_fields(&spec, raw_input).map_err(unavailable)?;
        let kind = ToolKind::from_name(spec.name).ok_or_else(|| unavailable(UNSUPPORTED))?;
        let input = Input::parse(kind, raw_input.clone()).map_err(unavailable)?;
        let project = self.runtime.1.project.clone();
        let raw_input = raw_input.clone();
        let (sender, receiver) = flume::bounded(1);
        let handle = host.runtime.handle().clone();
        let task = handle.spawn(async move {
            let result = prepare_example(host, project, input, raw_input).await;
            let _ = sender.send(result);
        });
        let result = receiver.recv_timeout(ANALYSIS_TIMEOUT);
        task.abort();
        let prepared = result
            .map_err(|_| unavailable("Host example analysis timed out or stopped"))?
            .map_err(unavailable)?;
        if let Some(path) = missing_read_target(&prepared.intent) {
            return Err(unavailable(format!(
                "{}: {path}",
                crate::MISSING_READ_TARGET
            )));
        }
        Ok(prepared)
    }
}

fn unavailable(reason: impl Into<String>) -> PermissionEditError {
    PermissionEditError::Unavailable(reason.into())
}

fn validate_runtime(runtime: &PermissionEditorRuntime) -> Result<(), PermissionEditError> {
    if !runtime.project.is_absolute() {
        return Err(unavailable(
            "The editor requires an absolute current project",
        ));
    }
    Ok(())
}

fn local_contract(source: &TrustedToolSource) -> Option<&str> {
    match source.subject() {
        PermissionSubject::Native { owner, contract }
            if source.builtin_allows() && owner == OWNER =>
        {
            Some(contract)
        }
        _ => None,
    }
}

fn descriptor(
    key: &str,
    source: TrustedToolSource,
    kind: Option<ToolKind>,
) -> EditableAuthorityDescriptor {
    let mut resources = Vec::new();
    let mut families = Vec::new();
    match kind {
        Some(ToolKind::Shell) => {
            let mut capability = resource(
                PermissionResourceKind::Command,
                vec![
                    SelectorMode::Exact,
                    SelectorMode::CommandPattern,
                    SelectorMode::CommandTemplate,
                    SelectorMode::Any,
                ],
                vec![PermissionResourceAccess::Execute],
            );
            capability.attributes.insert(
                WORKDIR_ATTRIBUTE.into(),
                vec![
                    SelectorMode::Exact,
                    SelectorMode::FilesystemSubtree,
                    SelectorMode::Any,
                ],
            );
            resources.push(capability);
        }
        Some(
            ToolKind::FileRead
            | ToolKind::FileGlob
            | ToolKind::FileGrep
            | ToolKind::Index
            | ToolKind::CodeMap
            | ToolKind::CodeContext
            | ToolKind::CodeRefs
            | ToolKind::CodeImpact
            | ToolKind::CodeExpand,
        ) => {
            resources.push(resource(
                PermissionResourceKind::File,
                vec![
                    SelectorMode::Exact,
                    SelectorMode::FilesystemSubtree,
                    SelectorMode::Any,
                ],
                vec![PermissionResourceAccess::Read],
            ));
            let mut directory = resource(
                PermissionResourceKind::Directory,
                vec![
                    SelectorMode::Exact,
                    SelectorMode::FilesystemSubtree,
                    SelectorMode::Any,
                ],
                vec![
                    PermissionResourceAccess::Read,
                    PermissionResourceAccess::List,
                    PermissionResourceAccess::Search,
                ],
            );
            directory
                .attributes
                .insert(BROWSE_RECURSION_ATTRIBUTE.into(), vec![SelectorMode::Exact]);
            resources.push(directory);
            families.push(PermissionCapabilityFamily::FilesystemRead);
            if matches!(kind, Some(ToolKind::FileRead | ToolKind::FileGlob)) {
                families.push(PermissionCapabilityFamily::FilesystemBrowse);
            }
        }
        Some(ToolKind::FileWrite | ToolKind::FileEdit | ToolKind::FileApplyPatch) => resources
            .push(resource(
                PermissionResourceKind::File,
                vec![
                    SelectorMode::Exact,
                    SelectorMode::FilesystemSubtree,
                    SelectorMode::Any,
                ],
                vec![PermissionResourceAccess::Write],
            )),
        Some(ToolKind::Webfetch) => resources.push(resource(
            PermissionResourceKind::Url,
            vec![
                SelectorMode::Exact,
                SelectorMode::UrlOrigin,
                SelectorMode::UrlSubtree,
                SelectorMode::Any,
            ],
            vec![PermissionResourceAccess::Read],
        )),
        Some(ToolKind::Websearch) => resources.push(resource(
            PermissionResourceKind::Query,
            vec![SelectorMode::Exact, SelectorMode::Any],
            vec![PermissionResourceAccess::Search],
        )),
        Some(ToolKind::Code | ToolKind::Environment) => resources.push(resource(
            PermissionResourceKind::Custom {
                name: if kind == Some(ToolKind::Code) {
                    "isolated_compute"
                } else {
                    "host_inspection"
                }
                .into(),
            },
            vec![SelectorMode::Exact, SelectorMode::Any],
            vec![if kind == Some(ToolKind::Code) {
                PermissionResourceAccess::Execute
            } else {
                PermissionResourceAccess::Read
            }],
        )),
        None => {}
    }
    EditableAuthorityDescriptor {
        key: key.into(),
        source,
        resources,
        arguments: vec![
            ArgumentMode::Exact,
            ArgumentMode::Selected,
            ArgumentMode::Unconstrained,
        ],
        families,
        unrestricted_resources: kind.is_some(),
        unavailable: None,
    }
}

fn resource(
    kind: PermissionResourceKind,
    selectors: Vec<SelectorMode>,
    access: Vec<PermissionResourceAccess>,
) -> ResourceCapability {
    ResourceCapability {
        kind,
        selectors,
        access,
        wildcard_access: true,
        wildcard_protection: true,
        attributes: BTreeMap::new(),
    }
}

pub(super) fn shell_plan_access(shell: &PreparedShell) -> PlanModeAccess {
    let read_only = shell.bash_program().ok().is_some_and(|program| {
        shell.bash_command_contexts().ok().is_some_and(|contexts| {
            let facts = pattern_analysis::shell_facts(program, &contexts);
            !facts.opaque
                && facts
                    .commands
                    .iter()
                    .all(|command| read_only_shell::scope_is_read_only(&command.scope))
        })
    });
    if read_only {
        PlanModeAccess::ReadOnly
    } else {
        PlanModeAccess::Prompted
    }
}

async fn prepare_example(
    host: Arc<HostInner>,
    project: PathBuf,
    input: Input,
    raw: Value,
) -> Result<PreparedInvocation, String> {
    let groups = host.project_groups(project.clone()).await?;
    match input {
        Input::Shell(input) => {
            let shell = groups
                .shell
                .prepare(input)
                .await
                .map_err(|error| error.to_string())?;
            shell_prepared(
                groups.shell,
                shell,
                &project,
                Some(&raw),
                ShellNativeRedirect::Off,
            )
            .map_err(|error| error.message)
        }
        Input::FileRead(input) => {
            let token = CancellationToken::new();
            let read = groups
                .files
                .prepare_read(input.clone(), &token)
                .await
                .map_err(|error| error.to_string())?;
            if read.resource().access != FileResourceAccess::Traverse {
                return Ok(file_read_prepared(&project, groups.files, read));
            }
            let group =
                FileToolGroup::new(&read.resource().path, false, Some(*groups.files.limits()))
                    .await
                    .map_err(|error| error.to_string())?;
            let read = group
                .prepare_read(
                    FileReadInput {
                        file_path: read.resource().path.to_string_lossy().into_owned(),
                        ..input
                    },
                    &token,
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(file_read_prepared(&project, group, read))
        }
        Input::FileGlob(mut input) => {
            let resource = groups
                .files
                .inspect_glob(&input)
                .await
                .map_err(|error| error.to_string())?;
            let (group, path) = confined_traversal_group(groups.files, &resource)
                .await
                .map_err(|error| error.to_string())?;
            input.path = Some(path);
            Ok(file_prepared(
                vec![resource],
                &project,
                group,
                Input::FileGlob(input),
                &["/pattern"],
            ))
        }
        Input::FileGrep(mut input) => {
            let resource = groups
                .files
                .inspect_grep(&input)
                .await
                .map_err(|error| error.to_string())?;
            let (group, path) = confined_traversal_group(groups.files, &resource)
                .await
                .map_err(|error| error.to_string())?;
            input.path = Some(path);
            Ok(file_prepared(
                vec![resource],
                &project,
                group,
                Input::FileGrep(input),
                &["/pattern", "/include"],
            ))
        }
        Input::FileWrite(input) => {
            let resource = groups
                .files
                .inspect_write(&input)
                .await
                .map_err(|error| error.to_string())?;
            Ok(file_prepared(
                vec![resource],
                &project,
                groups.files,
                Input::FileWrite(input),
                &[],
            ))
        }
        Input::FileEdit(input) => {
            let resource = groups
                .files
                .inspect_edit(&input)
                .await
                .map_err(|error| error.to_string())?;
            Ok(file_prepared(
                vec![resource],
                &project,
                groups.files,
                Input::FileEdit(input),
                &[],
            ))
        }
        Input::FileApplyPatch(input) => {
            let patch = groups
                .files
                .prepare_apply_patch(input, &CancellationToken::new())
                .await
                .map_err(|error| error.to_string())?;
            Ok(file_patch_prepared(
                patch.resources().to_vec(),
                &project,
                groups.files,
                patch,
            ))
        }
        Input::Index(mut input) => {
            input.path = expand_tilde(&input.path)?;
            let resource = groups
                .files
                .inspect_index(&input)
                .await
                .map_err(|error| error.to_string())?;
            Ok(index_prepared(resource, &project, groups.files))
        }
        Input::CodeMap(input) => Ok(code_graph_prepared(
            groups.code_graph,
            &project,
            input.path.as_deref(),
            &["/path"],
        )),
        Input::CodeContext(input) => Ok(code_graph_prepared(
            groups.code_graph,
            &project,
            input.path.as_deref(),
            &["/path", "/task"],
        )),
        Input::CodeRefs(input) => Ok(code_graph_prepared(
            groups.code_graph,
            &project,
            input.path.as_deref(),
            &["/path", "/symbol"],
        )),
        Input::CodeImpact(input) => Ok(code_graph_prepared(
            groups.code_graph,
            &project,
            input.path.as_deref(),
            &["/path", "/symbol"],
        )),
        Input::CodeExpand(input) => Ok(code_graph_prepared(
            groups.code_graph,
            &project,
            input.path.as_deref(),
            &["/path", "/symbol"],
        )),
        Input::Websearch(input) => {
            let prepared = host.web.prepare_websearch(input)?;
            Ok(PreparedInvocation {
                intent: web_intent(
                    PermissionResourceKind::Query,
                    prepared.permission_query.clone(),
                ),
                execution: PreparedExecution::Websearch(prepared),
                mutation_targets: Vec::new(),
                read_targets: Vec::new(),
            })
        }
        Input::Webfetch(input) => {
            let prepared = host
                .web
                .prepare_webfetch(input)
                .map_err(|error| error.to_string())?;
            Ok(PreparedInvocation {
                intent: web_intent(PermissionResourceKind::Url, prepared.permission_url.clone()),
                execution: PreparedExecution::Webfetch(prepared),
                mutation_targets: Vec::new(),
                read_targets: Vec::new(),
            })
        }
        Input::Code(_) => Ok(exact_custom_prepared(
            "isolated_compute",
            "python",
            PermissionResourceAccess::Execute,
            PermissionRisk::Low,
        )),
        Input::Environment => Ok(exact_custom_prepared(
            "host_inspection",
            "execution_environment",
            PermissionResourceAccess::Read,
            PermissionRisk::Medium,
        )),
    }
}

pub(super) fn web_intent(kind: PermissionResourceKind, value: String) -> PermissionIntent {
    let query = kind == PermissionResourceKind::Query;
    PermissionIntent::new(
        PermissionScopes::single(value.clone()),
        vec![PermissionResource {
            kind,
            value,
            access: Some(if query {
                PermissionResourceAccess::Search
            } else {
                PermissionResourceAccess::Read
            }),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }],
        if query {
            PermissionRisk::Low
        } else {
            PermissionRisk::Medium
        },
    )
    .with_authority(if query {
        PermissionAuthorityProfile::Query
    } else {
        PermissionAuthorityProfile::Url
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use caudra_agent::AgentMode;
    use caudra_agent::permissions::editor::{
        ArgumentsDraft, EffectivePolicyPreview, GuardDraft, IdentityDraft,
        PermissionAuthorityProvider, PermissionEditError, PermissionEditEvidence,
        PermissionEditOperation, PermissionRuleDraft, ProjectDraft, ResourceDraft, ResourcesDraft,
        SelectorDraft, SelectorValue, TemplateSource,
    };
    use caudra_agent::permissions::{
        COMMAND_OBSERVATION_ATTRIBUTE, PermissionLifetime, PermissionManager,
        PermissionResourceAccess, PermissionResourceKind, StructuredPermissionEffect,
        pattern_recognition::CommandObservation,
    };
    use caudra_agent::tools::{
        DescriptionContext, ParseError, PlanModeAccess, Tool, ToolAudience, ToolFilter,
        ToolInvocation, ToolRegistry, ToolSource,
    };
    use caudra_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_storage::StateDir;
    use caudra_storage::permission_patterns::{ArgumentRole, PatternToken};
    use caudra_storage::permission_state::PermissionState;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;
    use workcell::shell::{PreparedShell, ShellInput};

    use super::{
        DISABLED, NO_TEMPLATE, PermissionEditorContext, PermissionEditorRuntime, READ_ONLY,
        UNSUPPORTED, permission_authority_provider, shell_plan_access,
    };
    use crate::{WorkcellHost, pattern_analysis::shell_facts, read_only_shell};

    const SHELL: &str = "shell";
    const COMMAND: &str = "touch editor-sentinel";
    const PATTERN: &str = "touch *";
    const SENTINEL: &str = "editor-sentinel";
    const NEVER: &str = "Editor discovery must not run tool code";
    const CUSTOM_TOOL: &str = "untrusted_shell";
    const TEMPLATE_NAME: &str = "User literal template";
    const STATE_DIRECTORY: &str = "state";

    struct Fixture {
        root: TempDir,
        host: WorkcellHost,
        registry: Arc<ToolRegistry>,
        context: Arc<PermissionEditorContext>,
        provider: Arc<dyn PermissionAuthorityProvider>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let project = root.path().canonicalize().unwrap();
            let host = WorkcellHost::new(&project, None).unwrap();
            let registry = Arc::new(ToolRegistry::new());
            host.register(&registry).unwrap();
            let context = Arc::new(
                PermissionEditorContext::new(PermissionEditorRuntime {
                    project,
                    mode: AgentMode::Build,
                    tool_filter: ToolFilter::All,
                    audience: ToolAudience::MAIN,
                    workspace: None,
                })
                .unwrap(),
            );
            let provider =
                permission_authority_provider(registry.clone(), context.clone(), Some(&host));
            Self {
                root,
                host,
                registry,
                context,
                provider,
            }
        }

        fn project(&self) -> PathBuf {
            self.root.path().canonicalize().unwrap()
        }

        fn runtime(&self) -> PermissionEditorRuntime {
            self.context.current.read().unwrap().1.clone()
        }

        fn prepare_shell(&self, command: &str) -> PreparedShell {
            let host = &self.host.inner;
            let group = host
                .runtime
                .block_on(host.project_groups(self.project()))
                .unwrap()
                .shell;
            host.runtime
                .block_on(group.prepare(ShellInput {
                    command: command.into(),
                    timeout_sec: None,
                    workdir: Some(self.project().to_str().unwrap().into()),
                }))
                .unwrap()
        }

        fn permissions(&self) -> PermissionManager {
            let manager = PermissionManager::new_persistent_in(
                PermissionsConfig::default(),
                self.project(),
                Arc::default(),
                StateDir::from_path(self.project().join(STATE_DIRECTORY)),
            );
            manager.set_permission_authority_provider(self.provider.clone());
            manager
        }
    }

    struct NeverInvoked;
    impl Tool for NeverInvoked {
        fn name(&self) -> &str {
            CUSTOM_TOOL
        }
        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            panic!("{NEVER}")
        }
        fn schema(&self) -> Value {
            panic!("{NEVER}")
        }
        fn audience(&self) -> ToolAudience {
            panic!("{NEVER}")
        }
        fn parse(&self, _: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            panic!("{NEVER}")
        }
    }

    #[test_case(false; "untrusted_native")]
    #[test_case(true; "plugin_contract_is_not_native")]
    fn discovery_never_infers_authority_from_tool_code(plugin: bool) {
        let fixture = Fixture::new();
        let source = if plugin {
            ToolSource::Lua {
                plugin: "workcell".into(),
                contract: "shell.execution.v1".into(),
                bundled: false,
            }
        } else {
            ToolSource::Native {
                owner: "workcell".into(),
                contract: "shell.execution.v1".into(),
                trusted: false,
            }
        };
        fixture
            .registry
            .register(Arc::new(NeverInvoked), source)
            .unwrap();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == CUSTOM_TOOL)
            .unwrap();
        assert_eq!(authority.unavailable.as_deref(), Some(UNSUPPORTED));
        assert!(authority.resources.is_empty());
        assert!(authority.families.is_empty());
    }

    #[test_case(false; "disabled")]
    #[test_case(true; "read_only")]
    fn live_context_is_fenced_and_invalidates_catalog(read_only: bool) {
        let fixture = Fixture::new();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let previous = lease.catalog().clone();
        assert!(fixture.context.current.try_write().is_err());
        drop(lease);
        let mut runtime = fixture.runtime();
        if read_only {
            runtime.mode = AgentMode::ReadOnly;
        } else {
            runtime.tool_filter = ToolFilter::AllExcept(vec![SHELL.into()]);
        }
        fixture.context.replace(runtime).unwrap();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        assert_ne!(lease.catalog().revision, previous.revision);
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == SHELL)
            .unwrap();
        assert_eq!(
            authority.unavailable.as_deref(),
            Some(if read_only { READ_ONLY } else { DISABLED })
        );
        assert!(
            lease
                .analyze_example(authority, &json!({"command": COMMAND}))
                .is_err()
        );
    }

    #[test_case(ToolAudience::MAIN, true; "main")]
    #[test_case(ToolAudience::GENERAL_SUB, true; "general_subagent")]
    #[test_case(ToolAudience::RESEARCH_SUB, true; "research_subagent")]
    #[test_case(ToolAudience::INTERPRETER, false; "interpreter")]
    #[test_case(ToolAudience::all(), false; "all_is_not_one_agent_audience")]
    fn shell_analysis_preserves_dispatch_audience_restrictions(
        audience: ToolAudience,
        allowed: bool,
    ) {
        let fixture = Fixture::new();
        let mut runtime = fixture.runtime();
        runtime.audience = audience;
        fixture.context.replace(runtime).unwrap();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == SHELL)
            .unwrap();
        assert_eq!(
            authority.unavailable.as_deref(),
            (!allowed).then_some(DISABLED)
        );
        let result = lease.analyze_example(authority, &json!({"command": COMMAND}));
        if allowed {
            assert!(result.is_ok());
        } else {
            assert!(matches!(
                result,
                Err(PermissionEditError::Unavailable(reason)) if reason == DISABLED
            ));
        }
        assert!(!fixture.project().join(SENTINEL).exists());
    }

    #[test_case(false; "same_capabilities_different_mode")]
    #[test_case(true; "restored_mode_still_invalidates_review")]
    fn runtime_revision_prevents_context_aba(restored: bool) {
        let fixture = Fixture::new();
        let previous = fixture
            .provider
            .acquire(&fixture.project())
            .unwrap()
            .catalog()
            .clone();
        let mut runtime = fixture.runtime();
        runtime.mode = AgentMode::Plan(fixture.project().join("plan.md"));
        fixture.context.replace(runtime).unwrap();
        if restored {
            let mut runtime = fixture.runtime();
            runtime.mode = AgentMode::Build;
            fixture.context.replace(runtime).unwrap();
        }
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        assert_eq!(lease.catalog().authorities, previous.authorities);
        assert_ne!(lease.catalog().revision, previous.revision);
    }

    #[test]
    fn shell_analysis_neither_executes_nor_changes_the_standalone_operator_policy() {
        let fixture = Fixture::new();
        let host = &fixture.host.inner;
        let group = host
            .runtime
            .block_on(host.project_groups(fixture.project()))
            .unwrap()
            .shell;
        let input = ShellInput {
            command: COMMAND.into(),
            timeout_sec: None,
            workdir: Some(fixture.project().to_str().unwrap().into()),
        };
        let prepared = host.runtime.block_on(group.prepare(input.clone())).unwrap();
        let refusal = group.authorize_prepared(&prepared).unwrap_err();
        assert!(!prepared.policy_decision().is_allowed());
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == SHELL)
            .unwrap();
        lease
            .analyze_template(
                authority,
                &TemplateSource {
                    command: COMMAND.into(),
                    workdir: fixture.project(),
                },
            )
            .unwrap();
        lease
            .analyze_example(
                authority,
                &json!({"command": COMMAND, "workdir": fixture.project()}),
            )
            .unwrap();
        let group = host
            .runtime
            .block_on(host.project_groups(fixture.project()))
            .unwrap()
            .shell;
        let prepared = host.runtime.block_on(group.prepare(input)).unwrap();
        assert_eq!(group.authorize_prepared(&prepared), Err(refusal));
        assert!(!fixture.project().join(SENTINEL).exists());
    }

    #[test_case("git status --short", false, true; "read_only")]
    #[test_case("git diff HEAD~1", true, false; "unquoted_revision")]
    #[test_case("git diff 'HEAD~1'", false, true; "quoted_revision")]
    #[test_case("find . -name *.rs", true, false; "unquoted_glob")]
    #[test_case("find . -name '*.rs'", false, true; "quoted_glob")]
    #[test_case("rg a.*b src", true, false; "unquoted_regex")]
    #[test_case("rg 'a.*b' src", false, true; "quoted_regex")]
    #[test_case("git push origin main", false, false; "write")]
    #[test_case("git clean -n", false, false; "unsupported_dry_run")]
    #[test_case("touch editor-sentinel", false, false; "unknown_writer")]
    #[test_case("diff a b", false, false; "unknown_reader")]
    #[test_case("/bin/cat notes.md", false, false; "source_mismatch")]
    #[test_case("env git status", true, false; "env_wrapper")]
    #[test_case("command git status", true, false; "command_wrapper")]
    #[test_case("bash -c 'git status'", true, false; "shell_wrapper")]
    #[test_case("cat notes.md > editor-sentinel", true, true; "write_redirect")]
    #[test_case("cat < notes.md", true, true; "read_redirect")]
    #[test_case("cat notes.md 2>/dev/null", false, true; "null_redirect")]
    #[test_case("cat .env", false, true; "protected_not_checked_by_plan_access")]
    #[test_case("cat /etc/shadow", false, true; "confinement_not_checked_by_plan_access")]
    #[test_case("git status && rg needle src", false, true; "read_sequence")]
    #[test_case("git status | cat", false, true; "read_pipeline")]
    #[test_case("git status && touch editor-sentinel", false, false; "mixed_read_write")]
    #[test_case("cd - && cat notes.md", true, true; "unknown_cwd")]
    fn prepared_shell_plan_access_preserves_deterministic_facts(
        command: &str,
        opaque: bool,
        scopes_read_only: bool,
    ) {
        let fixture = Fixture::new();
        let prepared = fixture.prepare_shell(command);
        let program = prepared.bash_program().unwrap();
        let contexts = prepared.bash_command_contexts().unwrap();
        let facts = shell_facts(program, &contexts);
        assert_eq!(facts.opaque, opaque);
        assert!(!facts.commands.is_empty());
        assert_eq!(
            facts.commands.iter().all(|command| {
                read_only_shell::shell_read_only_verdict(&command.scope).is_ok()
            }),
            scopes_read_only
        );
        assert_eq!(
            shell_plan_access(&prepared),
            if !opaque && scopes_read_only {
                PlanModeAccess::ReadOnly
            } else {
                PlanModeAccess::Prompted
            }
        );
        assert!(!fixture.project().join(SENTINEL).exists());
    }

    #[test_case("cat notes.md", vec![true]; "confined_reader")]
    #[test_case("cat .env", vec![false]; "protected_operand")]
    #[test_case("cat /etc/shadow", vec![false]; "outside_operand")]
    #[test_case("cd src && cat notes.md", vec![true, true]; "known_cwd")]
    #[test_case("cd - && cat notes.md", vec![false, false]; "unknown_cwd")]
    fn prepared_scope_success_does_not_bypass_confinement(command: &str, confined: Vec<bool>) {
        let fixture = Fixture::new();
        let prepared = fixture.prepare_shell(command);
        let program = prepared.bash_program().unwrap();
        let contexts = prepared.bash_command_contexts().unwrap();
        let facts = shell_facts(program, &contexts);
        assert_eq!(
            facts
                .commands
                .iter()
                .map(|command| {
                    assert_eq!(
                        read_only_shell::shell_read_only_verdict(&command.scope),
                        Ok(())
                    );
                    read_only_shell::confined_read(
                        &command.scope,
                        &command.context.unwrap().incoming,
                        &fixture.project(),
                    )
                })
                .collect::<Vec<_>>(),
            confined
        );
    }

    #[test_case(COMMAND; "mutating_command_not_executed")]
    #[test_case("rg -n needle src"; "roles_from_host")]
    fn template_and_example_share_prepared_native_facts(command: &str) {
        let fixture = Fixture::new();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == SHELL)
            .unwrap();
        let source = TemplateSource {
            command: command.into(),
            workdir: fixture.project(),
        };
        let analysis = lease.analyze_template(authority, &source).unwrap();
        let example = lease
            .analyze_example(
                authority,
                &json!({"command": command, "workdir": source.workdir}),
            )
            .unwrap();
        let observation: CommandObservation = serde_json::from_str(
            &example.intent.resources[0].attributes[COMMAND_OBSERVATION_ATTRIBUTE],
        )
        .unwrap();
        assert_eq!(analysis.context, observation.context);
        assert_eq!(analysis.argv, observation.argv);
        assert_eq!(analysis.roles, observation.roles);
        assert_eq!(analysis.roles[0], ArgumentRole::Executable);
        assert!(analysis.option_like_data.is_empty());
        drop(lease);
        let manager = fixture.permissions();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let seeded = manager
            .seed_permission_template(&session, SHELL, &source, TEMPLATE_NAME)
            .unwrap();
        assert_eq!(seeded.context, analysis.context);
        assert_eq!(
            seeded.argv,
            analysis
                .argv
                .into_iter()
                .zip(analysis.roles)
                .map(|(value, role)| PatternToken::Exact { value, role })
                .collect::<Vec<_>>()
        );
        assert!(seeded.slots.is_empty());
        assert!(manager.pattern_proposal_inventory().2.is_empty());
        assert!(!fixture.project().join(SENTINEL).exists());
    }

    #[test_case("touch one; touch two"; "multiple_commands")]
    #[test_case("touch $(printf one)"; "dynamic_argument")]
    #[test_case("python -c 'print(1)'"; "interpreter")]
    fn unsupported_template_analysis_fails_closed(command: &str) {
        let fixture = Fixture::new();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == SHELL)
            .unwrap();
        let source = TemplateSource {
            command: command.into(),
            workdir: fixture.project(),
        };
        let error = lease.analyze_template(authority, &source).unwrap_err();
        assert!(
            matches!(error, PermissionEditError::Unavailable(ref reason) if reason == NO_TEMPLATE)
        );
        drop(lease);
        let manager = fixture.permissions();
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let error = manager
            .seed_permission_template(&session, SHELL, &source, TEMPLATE_NAME)
            .unwrap_err();
        assert!(
            matches!(error, PermissionEditError::Unavailable(ref reason) if reason == NO_TEMPLATE)
        );
        assert!(manager.pattern_proposal_inventory().2.is_empty());
    }

    #[test_case(false; "write")]
    #[test_case(true; "patch")]
    fn filesystem_examples_inspect_without_mutating(patch: bool) {
        const CONTENT: &str = "preview only\n";
        let fixture = Fixture::new();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let name = if patch {
            "file_apply_patch"
        } else {
            "file_write"
        };
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == name)
            .unwrap();
        let input = if patch {
            json!({"patchText": format!("*** Begin Patch\n*** Add File: {SENTINEL}\n+preview only\n*** End Patch")})
        } else {
            json!({"filePath": SENTINEL, "content": CONTENT})
        };
        let analysis = lease.analyze_example(authority, &input).unwrap();
        assert!(analysis.intent.resources.iter().any(|resource| {
            resource.value == fixture.project().join(SENTINEL).to_string_lossy()
                && resource.access == Some(PermissionResourceAccess::Write)
        }));
        assert!(!fixture.project().join(SENTINEL).exists());
    }

    #[test_case(false; "outside_plan")]
    #[test_case(true; "exact_plan")]
    fn file_examples_enforce_plan_target(exact_plan: bool) {
        let fixture = Fixture::new();
        let mut runtime = fixture.runtime();
        let plan = fixture.project().join("plan.md");
        runtime.mode = AgentMode::Plan(plan.clone());
        fixture.context.replace(runtime).unwrap();
        let lease = fixture.provider.acquire(&fixture.project()).unwrap();
        let authority = lease
            .catalog()
            .authorities
            .iter()
            .find(|entry| entry.key == "file_write")
            .unwrap();
        let target = if exact_plan {
            plan.clone()
        } else {
            fixture.project().join(SENTINEL)
        };
        let result = lease.analyze_example(
            authority,
            &json!({"filePath": target, "content": "preview only"}),
        );
        if exact_plan {
            assert_eq!(result.unwrap().plan_path, Some(plan));
        } else {
            assert!(
                matches!(result, Err(PermissionEditError::Unavailable(ref reason)) if reason == super::PLAN_RESTRICTED)
            );
        }
        assert!(!target.exists());
    }

    #[test_case(None, false; "allow_matches")]
    #[test_case(Some(Effect::Ask), false; "ask_still_prompts")]
    #[test_case(Some(Effect::Deny), false; "deny_still_denies")]
    #[test_case(None, true; "plan_ignores_persistent_allow")]
    fn arbitrary_example_distinguishes_match_from_policy(effect: Option<Effect>, planning: bool) {
        let fixture = Fixture::new();
        if planning {
            let mut runtime = fixture.runtime();
            runtime.mode = AgentMode::Plan(fixture.project().join("plan.md"));
            fixture.context.replace(runtime).unwrap();
        }
        let manager = PermissionManager::new_persistent_in(
            PermissionsConfig {
                rules: effect
                    .map(|effect| PermissionRule {
                        tool: ToolKey::native(SHELL),
                        scope: Some(PATTERN.into()),
                        effect,
                    })
                    .into_iter()
                    .collect(),
                ..PermissionsConfig::default()
            },
            fixture.project(),
            Arc::default(),
            StateDir::from_path(fixture.project().join(STATE_DIRECTORY)),
        );
        manager.set_permission_authority_provider(fixture.provider.clone());
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let draft = PermissionRuleDraft {
            identity: IdentityDraft::Registered {
                key: SHELL.into(),
                family: None,
            },
            effect: StructuredPermissionEffect::Allow,
            lifetime: PermissionLifetime::Project,
            project: ProjectDraft::Current,
            resources: ResourcesDraft::Constrained(vec![ResourceDraft {
                original_index: None,
                kind: PermissionResourceKind::Command,
                selector: SelectorDraft::Replace(SelectorValue::CommandPattern(PATTERN.into())),
                access: GuardDraft::Equals(PermissionResourceAccess::Execute),
                protected: GuardDraft::Equals(false),
                attributes: BTreeMap::new(),
            }]),
            arguments: ArgumentsDraft::Unconstrained,
            label: None,
        };
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let result = manager
            .preview_permission_example(&preview, SHELL, &json!({"command": COMMAND}))
            .unwrap();
        assert!(result.matches_rule);
        match effect {
            Some(Effect::Deny) => assert!(matches!(
                result.effective_policy,
                EffectivePolicyPreview::Denied(_)
            )),
            Some(Effect::Ask) => {
                assert_eq!(result.effective_policy, EffectivePolicyPreview::Prompt)
            }
            _ if planning => assert_eq!(result.effective_policy, EffectivePolicyPreview::Prompt),
            _ => assert_eq!(
                result.effective_policy,
                EffectivePolicyPreview::AllowedByPolicy
            ),
        }
        assert!(!fixture.project().join(SENTINEL).exists());
        assert!(manager.permission_edit_receipt(&preview).unwrap().is_none());
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        fixture.context.replace(fixture.runtime()).unwrap();
        assert!(matches!(
            manager.commit_permission_edit(&preview, &confirmation, &draft),
            Err(PermissionEditError::Conflict)
        ));
        assert!(manager.structured_rule_inventory().unwrap().is_empty());
        let session = manager
            .begin_permission_edit(
                PermissionEditOperation::Create,
                PermissionEditEvidence::default(),
            )
            .unwrap();
        let preview = manager.preview_permission_edit(&session, &draft).unwrap();
        let confirmation = preview.confirm(preview.requirements()).unwrap();
        let receipt = manager
            .commit_permission_edit(&preview, &confirmation, &draft)
            .unwrap();
        assert_eq!(
            manager.permission_edit_receipt(&preview).unwrap(),
            Some(receipt)
        );
        let saved = PermissionState::open(&StateDir::from_path(
            fixture.project().join(STATE_DIRECTORY),
        ))
        .unwrap();
        assert_eq!(saved.records().len(), 1);
        assert_eq!(saved.records()[0].rule, preview.normalized().unwrap().rule);
        assert!(!fixture.project().join(SENTINEL).exists());
    }
}
