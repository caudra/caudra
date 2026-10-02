use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Error;
use std::path::{Component, Path, PathBuf};

use caudra_storage::StateDir;
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::{DocumentRevision, LocalDocumentError, LocalDocumentStore};
use caudra_storage::plans::{MAX_PLAN_BYTES, PlanFile, validate_content};
use caudra_storage::private_file::PrivateFileError;
use caudra_storage::projects::project_subdir;
use caudra_workspace::{LocalDocumentRef, PlanRef, RecordScope, RecordedPath, WorkspaceSession};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AgentMode;
use crate::permissions::{
    PermissionResource, PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
};
use crate::tools::registry::{
    BoxFuture, ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent,
    PermissionScopes, Tool, ToolEffect, ToolError, ToolExecResult, ToolFailure, ToolInvocation,
};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::{TextOutput, ToolOutput};

pub const NAME: &str = "plan";
pub const DESCRIPTION: &str = "Read or replace the active plan document. Use action='read' to inspect it or action='write' with the complete content to save it. Only the main agent in plan mode may use this tool. The target is supplied by the host; paths and references are not accepted. Saving does not approve the plan or switch modes.";
pub const WRITE_RESULT_PREFIX: &str = "caudra_plan_write:";
const WRITE_RECEIPT: &str = "Active plan saved.";
const MAIN_ONLY: &str = "the plan tool is available only to the main agent";
const NO_TARGET: &str = "the plan tool requires an active plan target";
const REMOTE_REQUIRED: &str = "local document tools require a remote workspace session";
const STORE_UNAVAILABLE: &str = "local document store is unavailable";
const INVALID_TARGET: &str =
    "the active plan target is not a validated host workspace or owned plan path";
const CWD: &str = "{cwd}";
const PLANS_DIR: &str = "plans";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanTarget {
    Local(PathBuf),
    Remote(PlanRef),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanWriteResult {
    target: PlanTarget,
    revision: DocumentRevision,
    content: String,
}

impl PlanWriteResult {
    pub fn new(target: PlanTarget, revision: DocumentRevision, content: String) -> Self {
        Self {
            target,
            revision,
            content,
        }
    }

    pub fn target(&self) -> &PlanTarget {
        &self.target
    }

    pub fn revision(&self) -> &DocumentRevision {
        &self.revision
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn annotation(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self).map(|json| format!("{WRITE_RESULT_PREFIX}{json}"))
    }
}

pub fn parse_write_result(annotation: &str) -> Option<PlanWriteResult> {
    let result: PlanWriteResult =
        serde_json::from_str(annotation.strip_prefix(WRITE_RESULT_PREFIX)?).ok()?;
    validate_content(result.content()).ok()?;
    DocumentRevision::new(result.revision().as_str()).ok()?;
    match result.target() {
        PlanTarget::Local(path) if !valid_absolute_path(path) => return None,
        PlanTarget::Remote(reference) => {
            PlanRef::new(reference.as_str().to_owned()).ok()?;
        }
        _ => {}
    }
    Some(result)
}

pub struct PlanTool;

pub fn permission_contract() -> String {
    super::permission_contract(&PlanTool, ToolEffect::Mutating, DESCRIPTION)
}

pub struct PlanAuthority<'a> {
    pub mode: &'a AgentMode,
    pub host_cwd: &'a Path,
    pub audience: ToolAudience,
    pub workspace: Option<&'a WorkspaceSession>,
    pub local_documents: Option<&'a LocalDocumentStore>,
    pub session_id: Option<&'a SessionRef>,
}

impl<'a> PlanAuthority<'a> {
    fn from_context(ctx: &'a ToolContext, host_cwd: &'a Path) -> Self {
        Self {
            mode: &ctx.mode,
            host_cwd,
            audience: ctx.audience,
            workspace: ctx.workspace_session.as_ref(),
            local_documents: ctx.local_documents.as_deref(),
            session_id: ctx.session_id.as_ref(),
        }
    }

    pub fn verified_target(&self) -> Result<PlanTarget, ToolError> {
        let target = self.resolve_target()?;
        self.read_target(&target)?;
        Ok(target)
    }

    pub fn analyze(&self, input: &Value) -> Result<PermissionIntent, ToolError> {
        let call = PlanCall::parse(input)
            .map_err(|error| ToolError::new(ToolFailure::InvalidInput, error.to_string()))?;
        self.analyze_call(&call)
    }

    fn analyze_call(&self, call: &PlanCall) -> Result<PermissionIntent, ToolError> {
        let target = self.verified_target()?;
        let (kind, value, locator) = match target {
            PlanTarget::Local(path) => {
                let value = path.to_string_lossy().into_owned();
                (PermissionResourceKind::File, value.clone(), value)
            }
            PlanTarget::Remote(reference) => (
                PermissionResourceKind::Custom {
                    name: "local_document".to_owned(),
                },
                format!("plan:{}", reference.as_str()),
                reference.as_str().to_owned(),
            ),
        };
        Ok(PermissionIntent::new(
            PermissionScopes::single(format!("plan:{}:{locator}", call.operation())),
            vec![PermissionResource {
                kind,
                value,
                access: Some(if call.is_write() {
                    PermissionResourceAccess::Write
                } else {
                    PermissionResourceAccess::Read
                }),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([("operation".to_owned(), call.operation().to_owned())]),
            }],
            PermissionRisk::Low,
        ))
    }

    fn resolve_target(&self) -> Result<PlanTarget, ToolError> {
        if self.audience != ToolAudience::MAIN {
            return Err(ToolError::new(ToolFailure::Denied, MAIN_ONLY));
        }
        match self.mode {
            AgentMode::Plan(path) => {
                if !self.host_cwd.is_absolute()
                    || self
                        .host_cwd
                        .components()
                        .any(|part| part == Component::ParentDir)
                {
                    return Err(ToolError::new(ToolFailure::Denied, INVALID_TARGET));
                }
                let cwd: PathBuf = self.host_cwd.components().collect();
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    cwd.join(path)
                };
                if !valid_absolute_path(&path) {
                    return Err(ToolError::new(ToolFailure::Denied, INVALID_TARGET));
                }
                let path: PathBuf = path.components().collect();
                if !owned_plan_path(&path, &cwd)?
                    && !matches!(RecordedPath::of(&path, &cwd), RecordedPath::Inside(_))
                {
                    return Err(ToolError::new(ToolFailure::Denied, INVALID_TARGET));
                }
                Ok(PlanTarget::Local(path))
            }
            AgentMode::RemotePlan(reference) => Ok(PlanTarget::Remote(reference.clone())),
            _ => Err(ToolError::new(ToolFailure::Denied, NO_TARGET)),
        }
    }

    fn read_target(&self, target: &PlanTarget) -> Result<(String, DocumentRevision), ToolError> {
        match target {
            PlanTarget::Local(path) => PlanFile::new(path.clone())
                .and_then(|file| file.read())
                .map_err(storage_error),
            PlanTarget::Remote(reference) => {
                let store = self.remote_store()?;
                let document = store
                    .read(
                        store.project_key(),
                        self.session_id.map(SessionRef::as_str),
                        &LocalDocumentRef::Plan(reference.clone()),
                    )
                    .map_err(storage_error)?;
                Ok((document.content, document.revision))
            }
        }
    }

    fn remote_store(&self) -> Result<&'a LocalDocumentStore, ToolError> {
        let workspace = self
            .workspace
            .ok_or_else(|| ToolError::new(ToolFailure::Denied, REMOTE_REQUIRED))?;
        let store = self
            .local_documents
            .ok_or_else(|| ToolError::new(ToolFailure::Denied, STORE_UNAVAILABLE))?;
        store
            .validate_binding(workspace.binding())
            .map_err(storage_error)?;
        Ok(store)
    }
}

impl Tool for PlanTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        DESCRIPTION.into()
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["read", "write"]},
                "content": {"type": "string", "description": "Complete plan text, required for write.", "maxLength": MAX_PLAN_BYTES}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        Ok(Box::new(PlanCall::parse(input)?))
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum PlanCall {
    Read {},
    Write { content: String },
}

impl PlanCall {
    fn parse(input: &Value) -> Result<Self, ParseError> {
        let call: Self = serde_json::from_value(input.clone())
            .map_err(|error| ParseError::custom(error.to_string()))?;
        if let Self::Write { content } = &call {
            validate_content(content).map_err(|error| ParseError::custom(error.to_string()))?;
        }
        Ok(call)
    }

    fn is_write(&self) -> bool {
        matches!(self, Self::Write { .. })
    }

    fn operation(&self) -> &'static str {
        if self.is_write() { "write" } else { "read" }
    }
}

impl ToolInvocation for PlanCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(format!(
            "{} active plan",
            self.operation()
        )))
    }

    fn writes_active_plan(&self) -> bool {
        self.is_write()
    }

    fn call_effect(&self, _registered: ToolEffect) -> ToolEffect {
        if self.is_write() {
            ToolEffect::Mutating
        } else {
            ToolEffect::ReadOnly
        }
    }

    fn mutation_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        match resolve_target(ctx) {
            Ok(PlanTarget::Local(path)) if self.is_write() => vec![path],
            _ => Vec::new(),
        }
    }

    fn read_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        match resolve_target(ctx) {
            Ok(PlanTarget::Local(path)) if !self.is_write() => vec![path],
            _ => Vec::new(),
        }
    }

    fn record_scope(&self, ctx: &ToolContext, root: &Path) -> Option<RecordScope> {
        if !self.is_write() {
            return None;
        }
        let PlanTarget::Local(path) = resolve_target(ctx).ok()? else {
            return None;
        };
        if owned_plan_path(&path, &host_cwd(ctx)).ok()? {
            return None;
        }
        match RecordedPath::of(&path, root) {
            RecordedPath::Inside(path) => Some(RecordScope::Paths(BTreeSet::from([path]))),
            _ => None,
        }
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async move {
            PlanAuthority::from_context(ctx, &host_cwd(ctx))
                .analyze_call(self)
                .map(Some)
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            match execute(*self, ctx) {
                Ok(result) => result,
                Err(error) => ToolExecResult::failed(error.failure, error.message),
            }
        })
    }
}

pub fn verified_target(ctx: &ToolContext) -> Result<PlanTarget, ToolError> {
    PlanAuthority::from_context(ctx, &host_cwd(ctx)).verified_target()
}

fn resolve_target(ctx: &ToolContext) -> Result<PlanTarget, ToolError> {
    PlanAuthority::from_context(ctx, &host_cwd(ctx)).resolve_target()
}

fn host_cwd(ctx: &ToolContext) -> PathBuf {
    let cwd = ctx
        .host_cwd
        .clone()
        .unwrap_or_else(|| PathBuf::from(ctx.task_environment.apply(CWD).as_ref()));
    cwd.components().collect()
}

fn valid_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name().is_some()
        && path.to_str().is_some()
        && !path.components().any(|part| part == Component::ParentDir)
}

fn owned_plan_path(path: &Path, cwd: &Path) -> Result<bool, ToolError> {
    let state = StateDir::resolve_without_create().map_err(|error| error.to_string())?;
    let root = state
        .persistent_path()
        .join(project_subdir(cwd))
        .join(PLANS_DIR);
    Ok(path.strip_prefix(root).is_ok_and(|relative| {
        relative.file_name().is_some()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    }))
}

fn execute(call: PlanCall, ctx: &ToolContext) -> Result<ToolExecResult, ToolError> {
    let cwd = host_cwd(ctx);
    let authority = PlanAuthority::from_context(ctx, &cwd);
    let target = authority.verified_target()?;
    let PlanCall::Write { content } = call else {
        let (content, revision) = authority.read_target(&target)?;
        return Ok(
            ToolExecResult::from(Ok(ToolOutput::Markdown(content.into())))
                .with_annotation(Some(format!("revision {}", revision.as_str()))),
        );
    };
    let revision = match &target {
        PlanTarget::Local(path) => PlanFile::new(path.clone())
            .and_then(|file| file.write(&content))
            .map_err(storage_error)?,
        PlanTarget::Remote(reference) => {
            let store = authority.remote_store()?;
            store
                .write(
                    store.project_key(),
                    ctx.session_id.as_ref().map(|session| session.as_str()),
                    &LocalDocumentRef::Plan(reference.clone()),
                    &content,
                )
                .map_err(storage_error)?
        }
    };
    let written_path = match &target {
        PlanTarget::Local(path) => Some(path.to_string_lossy().into_owned()),
        PlanTarget::Remote(_) => None,
    };
    let saved = PlanWriteResult::new(target, revision, content);
    let state = saved.annotation().map_err(|error| error.to_string())?;
    let annotation = format!("revision {}", saved.revision().as_str());
    Ok(ToolExecResult::from(Ok(ToolOutput::Markdown(TextOutput {
        state: Some(Value::String(state)),
        ..saved.content.into()
    })))
    .with_model_output(Some(WRITE_RECEIPT.to_owned()))
    .with_annotation(Some(annotation))
    .with_written_path(written_path))
}

fn storage_error(error: LocalDocumentError) -> ToolError {
    let failure = match &error {
        LocalDocumentError::WrongProject
        | LocalDocumentError::SessionRequired
        | LocalDocumentError::WrongOwner
        | LocalDocumentError::Symlink => ToolFailure::Denied,
        LocalDocumentError::PrivateFile(error) => match error {
            PrivateFileError::UnsafePath
            | PrivateFileError::NotRegular
            | PrivateFileError::Permissions { .. }
            | PrivateFileError::DirectoryPermissions { .. } => ToolFailure::Denied,
            PrivateFileError::TooLarge => ToolFailure::InvalidInput,
            PrivateFileError::Io(kind) => ToolFailure::from(&Error::from(*kind)),
            _ => ToolFailure::Other,
        },
        _ => return error.into(),
    };
    ToolError::new(failure, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::slice;
    use std::sync::Arc;

    use caudra_config::FeatureFlags;
    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use caudra_storage::local_documents::LocalDocumentStore;
    use caudra_storage::plans::{MAX_PLAN_BYTES, PlanFile};
    use caudra_storage::projects::project_subdir;
    use caudra_workspace::{LocalDocumentRef, RecordScope};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        CWD, MAIN_ONLY, NAME, NO_TARGET, PLANS_DIR, PlanAuthority, PlanTarget, PlanTool,
        WRITE_RECEIPT, WRITE_RESULT_PREFIX, permission_contract, verified_target,
    };
    use crate::permissions::{PermissionResourceAccess, PermissionResourceKind};
    use crate::tools::native::batch::BatchTool;
    use crate::tools::native::local_document::tests::{tempdir, workspace_for_principal};
    use crate::tools::registry::{
        PermissionIntent, Tool, ToolEffect, ToolExecResult, ToolFailure, ToolSource,
    };
    use crate::tools::test_support::stub_ctx;
    use crate::tools::{ToolAudience, ToolContext};
    use crate::types::ToolOutput;
    use crate::{AgentEvent, AgentMode, EventSender};

    const CONTENT: &str = "# Plan\nDo the work.";
    const UPDATED: &str = "# Plan\nVerify the work.";
    const FILE_NAME: &str = "plan.md";
    const PRINCIPAL: &str = "principal";
    const OTHER_PRINCIPAL: &str = "other-principal";
    const MAX_LIVE_ANNOTATION_BYTES: usize = 80;
    const LARGE_CONTENT_REPEATS: usize = 4096;
    const BATCH_ID: &str = "plan-batch";
    #[cfg(unix)]
    const LEGACY_MODE: u32 = 0o644;

    fn authority<'a>(ctx: &'a ToolContext, cwd: &'a Path) -> PlanAuthority<'a> {
        PlanAuthority {
            mode: &ctx.mode,
            host_cwd: cwd,
            audience: ctx.audience,
            workspace: ctx.workspace_session.as_ref(),
            local_documents: ctx.local_documents.as_deref(),
            session_id: ctx.session_id.as_ref(),
        }
    }

    fn analyze_with_parity(ctx: &ToolContext, cwd: &Path, input: &Value) -> PermissionIntent {
        let authority = authority(ctx, cwd);
        assert_eq!(
            authority.verified_target().unwrap(),
            verified_target(ctx).unwrap()
        );
        let analyzed = authority.analyze(input).unwrap();
        let call = PlanTool.parse(input).unwrap();
        let preflight = smol::block_on(call.preflight(ctx)).unwrap().unwrap();
        assert_eq!(analyzed.resources, preflight.resources);
        assert_eq!(analyzed.scopes.scopes, preflight.scopes.scopes);
        assert_eq!(analyzed.scopes.force_prompt, preflight.scopes.force_prompt);
        assert_eq!(analyzed.scopes.plan_scoped, preflight.scopes.plan_scoped);
        assert_eq!(analyzed.risk, preflight.risk);
        assert_eq!(analyzed.authority, preflight.authority);
        analyzed
    }

    #[test]
    fn permission_contract_is_the_registered_native_identity() {
        let ctx = stub_ctx(&AgentMode::Build);
        crate::tools::native::register(&ctx.registry, FeatureFlags::all()).unwrap();
        let registered = ctx.registry.get(NAME).unwrap();
        let ToolSource::Native {
            owner,
            contract,
            trusted,
        } = registered.source
        else {
            panic!("the plan registration must retain its native identity");
        };
        assert_eq!(owner.as_ref(), crate::tools::native::OWNER);
        assert_eq!(contract.as_ref(), permission_contract());
        assert!(trusted);
        assert_eq!(registered.effect, ToolEffect::Mutating);
        assert_eq!(registered.tool.audience(), ToolAudience::MAIN);
    }

    #[test_case(false, false; "absolute_read")]
    #[test_case(false, true; "absolute_write")]
    #[test_case(true, false; "relative_read")]
    #[test_case(true, true; "relative_write")]
    fn local_authority_matches_preflight_without_creating_targets(relative: bool, write: bool) {
        let (root, mut ctx, path) = local();
        if relative {
            ctx.mode = AgentMode::Plan(PathBuf::from(FILE_NAME));
        }
        let input = if write {
            json!({"action":"write", "content":CONTENT})
        } else {
            json!({"action":"read"})
        };
        let analyzed = analyze_with_parity(&ctx, root.path(), &input);
        assert_eq!(analyzed.resources.len(), 1);
        assert_eq!(analyzed.resources[0].kind, PermissionResourceKind::File);
        assert_eq!(analyzed.resources[0].value, path.to_str().unwrap());
        assert_eq!(
            analyzed.resources[0].access,
            Some(if write {
                PermissionResourceAccess::Write
            } else {
                PermissionResourceAccess::Read
            })
        );
        assert!(!path.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn local_authority_analysis_never_chmods_or_creates_a_lock() {
        let (root, ctx, path) = local();
        fs::write(&path, CONTENT).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(LEGACY_MODE)).unwrap();
        let permissions = fs::metadata(&path).unwrap().permissions();
        for input in [
            json!({"action":"read"}),
            json!({"action":"write", "content":UPDATED}),
        ] {
            analyze_with_parity(&ctx, root.path(), &input);
        }
        assert_eq!(fs::metadata(&path).unwrap().permissions(), permissions);
        assert_eq!(fs::read_to_string(path).unwrap(), CONTENT);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test_case(false; "read")]
    #[test_case(true; "write")]
    fn remote_authority_matches_preflight_without_changing_the_document(write: bool) {
        let (root, ctx) = remote();
        let input = if write {
            json!({"action":"write", "content":CONTENT})
        } else {
            json!({"action":"read"})
        };
        let analyzed = analyze_with_parity(&ctx, root.path(), &input);
        let reference = ctx.mode.plan_ref().unwrap();
        assert_eq!(analyzed.resources.len(), 1);
        assert_eq!(
            analyzed.resources[0].kind,
            PermissionResourceKind::Custom {
                name: "local_document".to_owned()
            }
        );
        assert_eq!(
            analyzed.resources[0].value,
            format!("plan:{}", reference.as_str())
        );
        assert_eq!(
            analyzed.resources[0].access,
            Some(if write {
                PermissionResourceAccess::Write
            } else {
                PermissionResourceAccess::Read
            })
        );
        let store = ctx.local_documents.as_ref().unwrap();
        assert!(
            store
                .read(
                    store.project_key(),
                    ctx.session_id.as_ref().map(SessionRef::as_str),
                    &LocalDocumentRef::Plan(reference.clone())
                )
                .unwrap()
                .content
                .is_empty()
        );
    }

    #[test_case(AgentMode::Build, ToolAudience::MAIN; "build")]
    #[test_case(AgentMode::ReadOnly, ToolAudience::MAIN; "read_only")]
    #[test_case(AgentMode::Plan(PathBuf::from(FILE_NAME)), ToolAudience::GENERAL_SUB; "task")]
    #[test_case(AgentMode::Plan(PathBuf::from(FILE_NAME)), ToolAudience::RESEARCH_SUB; "research_task")]
    fn authority_and_preflight_refuse_the_same_invalid_context(
        mode: AgentMode,
        audience: ToolAudience,
    ) {
        let (root, mut ctx, _) = local();
        ctx.mode = mode;
        ctx.audience = audience;
        let input = json!({"action":"write", "content":CONTENT});
        let analyzed = authority(&ctx, root.path()).analyze(&input).unwrap_err();
        let preflight =
            smol::block_on(PlanTool.parse(&input).unwrap().preflight(&ctx)).unwrap_err();
        assert_eq!(analyzed.failure, ToolFailure::Denied);
        assert_eq!(analyzed.failure, preflight.failure);
        assert_eq!(analyzed.message, preflight.message);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test_case(true, false, false; "missing_workspace")]
    #[test_case(false, true, false; "missing_store")]
    #[test_case(false, false, true; "missing_session")]
    fn authority_requires_complete_remote_ownership(workspace: bool, store: bool, session: bool) {
        let (root, mut ctx) = remote();
        if workspace {
            ctx.workspace_session = None;
        }
        if store {
            ctx.local_documents = None;
        }
        if session {
            ctx.session_id = None;
        }
        let input = json!({"action":"write", "content":CONTENT});
        let analyzed = authority(&ctx, root.path()).analyze(&input).unwrap_err();
        let preflight =
            smol::block_on(PlanTool.parse(&input).unwrap().preflight(&ctx)).unwrap_err();
        assert_eq!(analyzed.failure, ToolFailure::Denied);
        assert_eq!(analyzed.failure, preflight.failure);
        assert_eq!(analyzed.message, preflight.message);
    }

    #[test_case("relative"; "relative_cwd")]
    #[test_case("/workspace/../other"; "cwd_traversal")]
    fn authority_refuses_unvalidated_host_cwd(cwd: &str) {
        let (root, mut ctx, _) = local();
        ctx.host_cwd = Some(PathBuf::from(cwd));
        let input = json!({"action":"write", "content":CONTENT});
        let analyzed = authority(&ctx, Path::new(cwd)).analyze(&input).unwrap_err();
        let preflight =
            smol::block_on(PlanTool.parse(&input).unwrap().preflight(&ctx)).unwrap_err();
        assert_eq!(analyzed.failure, ToolFailure::Denied);
        assert_eq!(analyzed.failure, preflight.failure);
        assert_eq!(analyzed.message, preflight.message);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    fn local() -> (TempDir, ToolContext, PathBuf) {
        let root = tempdir();
        let path = root.path().join(FILE_NAME);
        let mut ctx = stub_ctx(&AgentMode::Plan(path.clone()));
        ctx.task_environment = ctx.task_environment.set(CWD, root.path().to_str().unwrap());
        (root, ctx, path)
    }

    fn remote() -> (TempDir, ToolContext) {
        let root = tempdir();
        let workspace = workspace_for_principal(PRINCIPAL);
        let session = SessionRef::generate();
        let store = Arc::new(LocalDocumentStore::remote(
            StateDir::from_path(root.path().join("state")),
            workspace.binding(),
        ));
        let plan = store
            .create_plan(store.project_key(), session.as_str())
            .unwrap();
        let mut ctx = stub_ctx(&AgentMode::RemotePlan(plan));
        ctx.session_id = Some(session);
        ctx.workspace_session = Some(workspace);
        ctx.local_documents = Some(store);
        (root, ctx)
    }

    fn run(ctx: &ToolContext, input: Value) -> ToolExecResult {
        smol::block_on(PlanTool.parse(&input).unwrap().execute(ctx))
    }

    #[test_case(json!({"action":"read", "path":"plan.md"}); "no_path")]
    #[test_case(json!({"action":"read", "reference":"plan-a"}); "no_reference")]
    #[test_case(json!({"action":"read", "content":"text"}); "read_has_no_content")]
    #[test_case(json!({"action":"write"}); "write_requires_content")]
    #[test_case(json!({"action":"write", "content":true}); "write_requires_text")]
    #[test_case(json!({"action":"approve"}); "no_approval")]
    #[test_case(json!({"action":"patch"}); "no_patch")]
    #[test_case(json!({}); "action_required")]
    fn rejects_noncontract_arguments(input: Value) {
        assert!(PlanTool.parse(&input).is_err());
        let (root, ctx, _) = local();
        assert_eq!(
            authority(&ctx, root.path())
                .analyze(&input)
                .unwrap_err()
                .failure,
            ToolFailure::InvalidInput
        );
    }

    #[test_case(AgentMode::Build; "build")]
    #[test_case(AgentMode::ReadOnly; "read_only")]
    fn refuses_without_an_active_plan(mode: AgentMode) {
        let ctx = stub_ctx(&mode);
        for input in [
            json!({"action":"read"}),
            json!({"action":"write", "content":CONTENT}),
        ] {
            let call = PlanTool.parse(&input).unwrap();
            let preflight = smol::block_on(call.preflight(&ctx)).unwrap_err();
            assert_eq!(preflight.failure, ToolFailure::Denied);
            assert_eq!(preflight.message, NO_TARGET);
            let result = smol::block_on(call.execute(&ctx));
            assert_eq!(result.failure, Some(ToolFailure::Denied));
            assert_eq!(result.output.unwrap_err(), NO_TARGET);
        }
    }

    #[test_case(ToolAudience::GENERAL_SUB; "task")]
    #[test_case(ToolAudience::RESEARCH_SUB; "research_task")]
    fn only_main_can_read_or_write(audience: ToolAudience) {
        let (_root, mut ctx, path) = local();
        ctx.audience = audience;
        for input in [
            json!({"action":"read"}),
            json!({"action":"write", "content":CONTENT}),
        ] {
            let result = run(&ctx, input);
            assert_eq!(result.failure, Some(ToolFailure::Denied));
            assert_eq!(result.output.unwrap_err(), MAIN_ONLY);
        }
        assert!(!path.exists());
    }

    #[test_case(false; "absolute")]
    #[test_case(true; "legacy_relative")]
    fn local_round_trip_retains_committed_content_and_exact_record_scope(relative: bool) {
        let (root, mut ctx, path) = local();
        if relative {
            ctx.mode = AgentMode::Plan(PathBuf::from(FILE_NAME));
        }
        let write = PlanTool
            .parse(&json!({"action":"write", "content":CONTENT}))
            .unwrap();
        let read = PlanTool.parse(&json!({"action":"read"})).unwrap();
        assert_eq!(
            write.call_effect(ToolEffect::Mutating),
            ToolEffect::Mutating
        );
        assert_eq!(read.call_effect(ToolEffect::Mutating), ToolEffect::ReadOnly);
        assert!(write.writes_active_plan());
        assert!(!read.writes_active_plan());
        assert_eq!(write.mutation_targets(&ctx), slice::from_ref(&path));
        assert!(write.read_targets(&ctx).is_empty());
        assert_eq!(read.read_targets(&ctx), slice::from_ref(&path));
        assert!(read.mutation_targets(&ctx).is_empty());
        let Some(RecordScope::Paths(paths)) = write.record_scope(&ctx, root.path()) else {
            panic!("an ordinary workspace target must record exactly one path");
        };
        assert_eq!(paths.len(), 1);
        assert_eq!(paths.first().unwrap().as_str(), FILE_NAME);
        let intent = smol::block_on(write.preflight(&ctx)).unwrap().unwrap();
        assert_eq!(intent.resources[0].kind, PermissionResourceKind::File);
        assert_eq!(
            intent.resources[0].access,
            Some(PermissionResourceAccess::Write)
        );
        assert_eq!(intent.resources[0].value, path.to_str().unwrap());
        assert!(!path.exists());
        let result = smol::block_on(write.execute(&ctx));
        assert_eq!(result.model_output.as_deref(), Some(WRITE_RECEIPT));
        assert_eq!(result.written_path.as_deref(), path.to_str());
        let output = result.output.unwrap();
        assert_eq!(output.as_text(), CONTENT);
        let persisted = serde_json::to_string(&output).unwrap();
        let restored: ToolOutput = serde_json::from_str(&persisted).unwrap();
        let saved = restored.plan_write_result().unwrap();
        let annotation = result.annotation.unwrap();
        assert_eq!(
            annotation,
            format!("revision {}", saved.revision().as_str())
        );
        assert!(annotation.len() <= MAX_LIVE_ANNOTATION_BYTES);
        assert!(!annotation.contains(WRITE_RESULT_PREFIX));
        assert_eq!(saved.target(), &PlanTarget::Local(path.clone()));
        assert_eq!(saved.content(), CONTENT);
        assert_eq!(
            saved.revision(),
            &PlanFile::new(path.clone()).unwrap().read().unwrap().1
        );
        let update = run(&ctx, json!({"action":"write", "content":UPDATED}));
        let updated = update.output.unwrap().plan_write_result().unwrap();
        assert_ne!(saved.revision(), updated.revision());
        assert_eq!(saved.content(), CONTENT);
        assert_eq!(
            smol::block_on(read.execute(&ctx)).output.unwrap().as_text(),
            UPDATED
        );
    }

    #[test]
    fn remote_round_trip_uses_owned_state_not_workspace_recording() {
        let (_root, ctx) = remote();
        let call = PlanTool
            .parse(&json!({"action":"write", "content":CONTENT}))
            .unwrap();
        assert!(call.record_scope(&ctx, Path::new("/")).is_none());
        assert!(call.mutation_targets(&ctx).is_empty());
        let intent = smol::block_on(call.preflight(&ctx)).unwrap().unwrap();
        assert!(matches!(
            intent.resources[0].kind,
            PermissionResourceKind::Custom { .. }
        ));
        assert_eq!(
            intent.resources[0].access,
            Some(PermissionResourceAccess::Write)
        );
        let result = smol::block_on(call.execute(&ctx));
        assert!(!result.is_error);
        assert!(result.written_path.is_none());
        assert_eq!(result.model_output.as_deref(), Some(WRITE_RECEIPT));
        let output = result.output.unwrap();
        let restored: ToolOutput =
            serde_json::from_value(serde_json::to_value(&output).unwrap()).unwrap();
        let saved = restored.plan_write_result().unwrap();
        assert_eq!(
            result.annotation,
            Some(format!("revision {}", saved.revision().as_str()))
        );
        let reference = ctx.mode.plan_ref().unwrap();
        assert_eq!(saved.target(), &PlanTarget::Remote(reference.clone()));
        assert_eq!(saved.content(), CONTENT);
        let store = ctx.local_documents.as_ref().unwrap();
        let document = store
            .read(
                store.project_key(),
                ctx.session_id.as_ref().map(SessionRef::as_str),
                &LocalDocumentRef::Plan(reference.clone()),
            )
            .unwrap();
        assert_eq!(saved.revision(), &document.revision);
        assert_eq!(
            run(&ctx, json!({"action":"read"}))
                .output
                .unwrap()
                .as_text(),
            CONTENT
        );
    }

    #[test_case(false; "wrong_session")]
    #[test_case(true; "wrong_binding")]
    fn remote_owner_validation_is_a_typed_refusal(binding: bool) {
        let (root, mut ctx) = remote();
        if binding {
            ctx.workspace_session = Some(workspace_for_principal(OTHER_PRINCIPAL));
        } else {
            ctx.session_id = Some(SessionRef::generate());
        }
        let call = PlanTool
            .parse(&json!({"action":"write", "content":CONTENT}))
            .unwrap();
        assert_eq!(
            authority(&ctx, root.path())
                .verified_target()
                .unwrap_err()
                .failure,
            ToolFailure::Denied
        );
        assert_eq!(
            authority(&ctx, root.path())
                .analyze(&json!({"action":"write", "content":CONTENT}))
                .unwrap_err()
                .failure,
            ToolFailure::Denied
        );
        assert_eq!(
            smol::block_on(call.preflight(&ctx)).unwrap_err().failure,
            ToolFailure::Denied
        );
        assert_eq!(
            smol::block_on(call.execute(&ctx)).failure,
            Some(ToolFailure::Denied)
        );
    }

    #[test]
    fn owned_local_state_never_records_a_workspace() {
        let (root, mut ctx, _) = local();
        let state = StateDir::resolve_without_create().unwrap();
        let path = state
            .persistent_path()
            .join(project_subdir(root.path()))
            .join(PLANS_DIR)
            .join(FILE_NAME);
        ctx.mode = AgentMode::Plan(path.clone());
        let call = PlanTool
            .parse(&json!({"action":"write", "content":CONTENT}))
            .unwrap();
        assert_eq!(call.mutation_targets(&ctx), [path]);
        assert!(call.record_scope(&ctx, state.persistent_path()).is_none());
    }

    #[test_case("../outside.md"; "traversal")]
    #[test_case("."; "workspace_root")]
    #[test_case(""; "empty_path")]
    fn unclassifiable_targets_refuse_without_workspace_fallback(target: &str) {
        let (root, mut ctx, _) = local();
        ctx.mode = AgentMode::Plan(PathBuf::from(target));
        let call = PlanTool
            .parse(&json!({"action":"write", "content":CONTENT}))
            .unwrap();
        assert!(verified_target(&ctx).is_err());
        assert!(call.record_scope(&ctx, root.path()).is_none());
        assert!(call.mutation_targets(&ctx).is_empty());
    }

    #[test]
    fn oversized_content_is_rejected_before_storage() {
        let (_root, ctx, path) = local();
        assert!(
            PlanTool
                .parse(&json!({"action":"write", "content":"x".repeat(MAX_PLAN_BYTES + 1)}))
                .is_err()
        );
        PlanFile::new(path.clone()).unwrap().write("").unwrap();
        fs::write(&path, "x".repeat(MAX_PLAN_BYTES + 1)).unwrap();
        let result = run(&ctx, json!({"action":"read"}));
        assert_eq!(result.failure, Some(ToolFailure::InvalidInput));
    }

    #[test_case(false; "local")]
    #[test_case(true; "remote")]
    fn large_plan_results_keep_live_and_model_annotations_bounded(remote_target: bool) {
        let (_root, mut ctx) = if remote_target {
            remote()
        } else {
            let (root, ctx, _) = local();
            (root, ctx)
        };
        let content = CONTENT.repeat(LARGE_CONTENT_REPEATS);
        let result = run(&ctx, json!({"action":"write", "content":content}));
        assert_eq!(result.model_output.as_deref(), Some(WRITE_RECEIPT));
        let output = result.output.unwrap();
        let saved = output.plan_write_result().unwrap();
        assert_eq!(saved.content(), content);
        assert!(result.annotation.as_ref().unwrap().len() <= MAX_LIVE_ANNOTATION_BYTES);
        assert!(
            !result
                .annotation
                .as_ref()
                .unwrap()
                .contains(WRITE_RESULT_PREFIX)
        );
        assert!(!result.model_output.unwrap().contains(WRITE_RESULT_PREFIX));
        let read = run(&ctx, json!({"action":"read"}));
        assert_eq!(read.output.unwrap().as_text(), content);
        assert!(read.model_output.is_none());
        assert!(read.annotation.unwrap().len() <= MAX_LIVE_ANNOTATION_BYTES);

        crate::tools::native::register(&ctx.registry, FeatureFlags::all()).unwrap();
        let (tx, rx) = flume::unbounded();
        ctx.event_tx = EventSender::new(tx, 0);
        ctx.tool_use_id = Some(BATCH_ID.to_owned());
        let result = smol::block_on(BatchTool.parse(&json!({
            "tool_calls": [{"tool": NAME, "parameters": {"action":"write", "content":content}}]
        })).unwrap().execute(&ctx));
        assert!(!result.is_error);
        let output = result.output.unwrap();
        let restored: ToolOutput =
            serde_json::from_value(serde_json::to_value(&output).unwrap()).unwrap();
        let ToolOutput::Batch { entries, text } = restored else {
            panic!("the batch result must retain its child output");
        };
        let entry = &entries[0];
        let saved = entry.output.as_ref().unwrap().plan_write_result().unwrap();
        assert_eq!(saved.content(), content);
        assert_eq!(
            entry.annotation,
            Some(format!("revision {}", saved.revision().as_str()))
        );
        assert!(entry.annotation.as_ref().unwrap().len() <= MAX_LIVE_ANNOTATION_BYTES);
        assert!(!text.contains(WRITE_RESULT_PREFIX));
        let live = rx
            .drain()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::BatchProgress(progress) => Some(progress.entry),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(live.iter().any(|entry| {
            entry
                .output
                .as_ref()
                .and_then(ToolOutput::plan_write_result)
                .is_some()
        }));
        for entry in live {
            if let Some(annotation) = entry.annotation {
                assert!(annotation.len() <= MAX_LIVE_ANNOTATION_BYTES);
                assert!(!annotation.contains(WRITE_RESULT_PREFIX));
            }
        }
    }
}
