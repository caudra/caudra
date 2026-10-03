use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Error;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use caudra_storage::StateDir;
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::{LocalDocumentError, LocalDocumentStore};
use caudra_storage::plans::{MAX_PLAN_BYTES, PlanFile, validate_content};
use caudra_storage::private_file::PrivateFileError;
use caudra_storage::projects::project_subdir;
use caudra_storage::sessions::StoredPlanTarget;
use caudra_workspace::{LocalDocumentRef, PlanRef, RecordScope, RecordedPath, WorkspaceSession};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AgentMode;
use crate::permissions::{
    PermissionResource, PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
    PermissionSubject,
};
use crate::tools::registry::{
    BoxFuture, ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent,
    PermissionScopes, Tool, ToolEffect, ToolError, ToolExecResult, ToolFailure, ToolInvocation,
};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::{TextOutput, ToolOutput};

pub const NAME: &str = "plan";
pub const DESCRIPTION: &str = "Read or replace this session's plan. Use action='read' to inspect it or action='write' with the complete content to save it. Any agent may read the plan; only the main agent may replace it. The target is supplied by the host; paths and references are not accepted. Saving does not approve the plan or switch modes.";
pub const WRITE_RESULT_PREFIX: &str = "caudra_plan_write:";
/// What a person is shown in place of a remote plan's reference, which means
/// nothing to them: a session has one plan.
pub const SESSION_PLAN_LABEL: &str = "this session's plan";
const WRITE_RECEIPT: &str = "Active plan saved.";
pub(crate) const WRITE_DENIED: &str =
    "only the main agent may replace the session plan, and not in read-only mode";
const NO_TARGET: &str = "the plan tool requires a session plan";
const REMOTE_REQUIRED: &str = "local document tools require a remote workspace session";
const STORE_UNAVAILABLE: &str = "local document store is unavailable";
const INVALID_TARGET: &str =
    "the active plan target is not a validated host workspace or owned plan path";
const CWD: &str = "{cwd}";
const PLANS_DIR: &str = "plans";

static PERMISSION_CONTRACT: LazyLock<String> =
    LazyLock::new(|| super::permission_contract(&PlanTool, ToolEffect::Mutating, DESCRIPTION));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAccess {
    Read,
    Write,
}

impl PlanAccess {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    pub fn resource_access(self) -> PermissionResourceAccess {
        match self {
            Self::Read => PermissionResourceAccess::Read,
            Self::Write => PermissionResourceAccess::Write,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanTarget {
    Local(PathBuf),
    Remote(PlanRef),
}

impl From<&StoredPlanTarget> for PlanTarget {
    fn from(target: &StoredPlanTarget) -> Self {
        match target {
            StoredPlanTarget::LocalPath { path } => Self::Local(PathBuf::from(path)),
            StoredPlanTarget::PlanRef { reference } => Self::Remote(reference.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanWriteResult {
    target: PlanTarget,
    content: String,
}

impl PlanWriteResult {
    pub fn new(target: PlanTarget, content: String) -> Self {
        Self { target, content }
    }

    pub fn target(&self) -> &PlanTarget {
        &self.target
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

pub fn permission_contract() -> &'static str {
    &PERMISSION_CONTRACT
}

/// Keyed on the trusted identity rather than the name, so a plugin tool that
/// happens to be called `plan` is never mistaken for the session plan.
pub fn is_plan_subject(subject: &PermissionSubject) -> bool {
    matches!(subject, PermissionSubject::Native { owner, contract }
        if owner == super::OWNER && contract == permission_contract())
}

pub struct PlanAuthority<'a> {
    pub mode: &'a AgentMode,
    /// The session's plan, which a planning mode's own target overrides.
    pub plan: Option<&'a PlanTarget>,
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
            plan: ctx.plan.as_ref(),
            host_cwd,
            audience: ctx.audience,
            workspace: ctx.workspace_session.as_ref(),
            local_documents: ctx.local_documents.as_deref(),
            session_id: ctx.session_id.as_ref(),
        }
    }

    pub fn verified_target(&self) -> Result<PlanTarget, ToolError> {
        self.verified(PlanAccess::Read)
    }

    pub fn analyze(&self, input: &Value) -> Result<PermissionIntent, ToolError> {
        let call = PlanCall::parse(input)
            .map_err(|error| ToolError::new(ToolFailure::InvalidInput, error.to_string()))?;
        self.analyze_call(&call)
    }

    fn verified(&self, access: PlanAccess) -> Result<PlanTarget, ToolError> {
        let target = self.resolve_target(access)?;
        self.read_target(&target)?;
        Ok(target)
    }

    fn analyze_call(&self, call: &PlanCall) -> Result<PermissionIntent, ToolError> {
        let target = self.verified(call.access())?;
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
                access: Some(call.access().resource_access()),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([("operation".to_owned(), call.operation().to_owned())]),
            }],
            PermissionRisk::Low,
        ))
    }

    /// Any audience with a session plan may read it, but only the main agent
    /// outside read-only mode may replace it. Refusing here keeps a task's
    /// write ahead of every permission and storage step.
    fn resolve_target(&self, access: PlanAccess) -> Result<PlanTarget, ToolError> {
        if access == PlanAccess::Write
            && (self.audience != ToolAudience::MAIN || self.mode.is_read_only())
        {
            return Err(ToolError::new(ToolFailure::Denied, WRITE_DENIED));
        }
        let planning = self.mode.plan_target();
        match planning.as_ref().or(self.plan) {
            Some(PlanTarget::Local(path)) => self.local_target(path).map(PlanTarget::Local),
            Some(PlanTarget::Remote(reference)) => Ok(PlanTarget::Remote(reference.clone())),
            None => Err(ToolError::new(ToolFailure::Denied, NO_TARGET)),
        }
    }

    fn local_target(&self, path: &Path) -> Result<PathBuf, ToolError> {
        if !self.host_cwd.is_absolute()
            || self
                .host_cwd
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Err(ToolError::new(ToolFailure::Denied, INVALID_TARGET));
        }
        let cwd: PathBuf = self.host_cwd.components().collect();
        let path = cwd.join(path);
        if !valid_absolute_path(&path) {
            return Err(ToolError::new(ToolFailure::Denied, INVALID_TARGET));
        }
        let path: PathBuf = path.components().collect();
        if !owned_plan_path(&path, &cwd)?
            && !matches!(RecordedPath::of(&path, &cwd), RecordedPath::Inside(_))
        {
            return Err(ToolError::new(ToolFailure::Denied, INVALID_TARGET));
        }
        Ok(path)
    }

    fn read_target(&self, target: &PlanTarget) -> Result<String, ToolError> {
        match target {
            PlanTarget::Local(path) => PlanFile::new(path.clone())
                .and_then(|file| file.read())
                .map_err(storage_error),
            PlanTarget::Remote(reference) => {
                let store = self.remote_store()?;
                store
                    .read(
                        store.project_key(),
                        self.session_id.map(SessionRef::as_str),
                        &LocalDocumentRef::Plan(reference.clone()),
                    )
                    .map(|document| document.content)
                    .map_err(storage_error)
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
        ToolAudience::all()
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

    fn has_read_only_calls(&self) -> bool {
        true
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

    fn access(&self) -> PlanAccess {
        match self {
            Self::Read {} => PlanAccess::Read,
            Self::Write { .. } => PlanAccess::Write,
        }
    }

    fn is_write(&self) -> bool {
        self.access() == PlanAccess::Write
    }

    fn operation(&self) -> &'static str {
        self.access().operation()
    }
}

impl ToolInvocation for PlanCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(format!(
            "{} active plan",
            self.operation()
        )))
    }

    fn active_plan_access(&self) -> Option<PlanAccess> {
        Some(self.access())
    }

    fn call_effect(&self, _registered: ToolEffect) -> ToolEffect {
        if self.is_write() {
            ToolEffect::Mutating
        } else {
            ToolEffect::ReadOnly
        }
    }

    fn mutation_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        match write_target(ctx) {
            Ok(PlanTarget::Local(path)) if self.is_write() => vec![path],
            _ => Vec::new(),
        }
    }

    fn read_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        match resolve_target(ctx, PlanAccess::Read) {
            Ok(PlanTarget::Local(path)) if !self.is_write() => vec![path],
            _ => Vec::new(),
        }
    }

    fn record_scope(&self, ctx: &ToolContext, root: &Path) -> Option<RecordScope> {
        if !self.is_write() {
            return None;
        }
        let PlanTarget::Local(path) = write_target(ctx).ok()? else {
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

/// The target a main-agent plan write replaces. A task or read-only write is
/// refused here exactly as preflight and execution refuse it.
pub(crate) fn write_target(ctx: &ToolContext) -> Result<PlanTarget, ToolError> {
    resolve_target(ctx, PlanAccess::Write)
}

fn resolve_target(ctx: &ToolContext, access: PlanAccess) -> Result<PlanTarget, ToolError> {
    PlanAuthority::from_context(ctx, &host_cwd(ctx)).resolve_target(access)
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
    let target = authority.verified(call.access())?;
    let PlanCall::Write { content } = call else {
        let content = authority.read_target(&target)?;
        return Ok(ToolExecResult::from(Ok(ToolOutput::Markdown(
            content.into(),
        ))));
    };
    let written_path = match &target {
        PlanTarget::Local(path) => {
            PlanFile::new(path.clone())
                .and_then(|file| file.write(&content))
                .map_err(storage_error)?;
            Some(path.to_string_lossy().into_owned())
        }
        PlanTarget::Remote(reference) => {
            let store = authority.remote_store()?;
            store
                .write(
                    store.project_key(),
                    ctx.session_id.as_ref().map(|session| session.as_str()),
                    &LocalDocumentRef::Plan(reference.clone()),
                    &content,
                )
                .map_err(storage_error)?;
            None
        }
    };
    let saved = PlanWriteResult::new(target, content);
    let state = saved.annotation().map_err(|error| error.to_string())?;
    Ok(ToolExecResult::from(Ok(ToolOutput::Markdown(TextOutput {
        state: Some(Value::String(state)),
        ..saved.content.into()
    })))
    .with_model_output(Some(WRITE_RECEIPT.to_owned()))
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
    use std::borrow::Cow;
    use std::collections::BTreeSet;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::slice;
    use std::sync::Arc;

    use caudra_config::{
        DefaultEffect, Effect, FeatureFlags, PermissionRule, PermissionsConfig, ToolKey,
    };
    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use caudra_storage::local_documents::LocalDocumentStore;
    use caudra_storage::plans::{MAX_PLAN_BYTES, PlanFile};
    use caudra_storage::projects::project_subdir;
    use caudra_workspace::{LocalDocumentRef, PlanRef, RecordScope};
    use futures_lite::future::poll_once;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        CWD, INVALID_TARGET, NAME, NO_TARGET, PLANS_DIR, PlanAccess, PlanAuthority, PlanTarget,
        PlanTool, PlanWriteResult, STORE_UNAVAILABLE, WRITE_DENIED, WRITE_RECEIPT,
        WRITE_RESULT_PREFIX, host_cwd, parse_write_result, permission_contract, verified_target,
        write_target,
    };
    use crate::agent::tool_dispatch::{self, Emit, TOOL_DISABLED_SUFFIX};
    use crate::permissions::{
        BOUNDARY_UNVERIFIABLE_PREFIX, PERMISSION_DENIED_PREFIX, PermissionAnswer,
        PermissionManager, PermissionResourceAccess, PermissionResourceKind,
    };
    use crate::tools::native::batch::BatchTool;
    use crate::tools::native::tests::{tempdir, workspace_for_principal};
    use crate::tools::registry::{
        ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent, Tool, ToolEffect,
        ToolExecResult, ToolFailure, ToolInvocation, ToolSource,
    };
    use crate::tools::test_support::{NamedMock, stub_ctx};
    use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
    use crate::types::ToolOutput;
    use crate::{AgentEvent, AgentMode, Envelope, EventSender};

    const CONTENT: &str = "# Plan\nDo the work.";
    const UPDATED: &str = "# Plan\nVerify the work.";
    const FILE_NAME: &str = "plan.md";
    const PRINCIPAL: &str = "principal";
    const OTHER_PRINCIPAL: &str = "other-principal";
    const ABSOLUTE_PLAN: &str = "/workspace/plan.md";
    const REMOTE_PLAN: &str = "plan-current";
    const ENCODED_FIELDS: [&str; 2] = ["content", "target"];
    const LARGE_CONTENT_REPEATS: usize = 4096;
    const BATCH_ID: &str = "plan-batch";
    const OTHER_FILE_NAME: &str = "other.md";
    const CLAIM_NAME: &str = "plan_claim";
    #[cfg(unix)]
    const LEGACY_MODE: u32 = 0o644;

    enum Session {
        Plan,
        RemotePlan,
        Build,
        RemoteBuild,
        Unbound,
        ReadOnly,
    }

    #[derive(PartialEq)]
    enum Approval {
        Automatic,
        Refused,
        Asked,
    }

    fn authority<'a>(ctx: &'a ToolContext, cwd: &'a Path) -> PlanAuthority<'a> {
        PlanAuthority {
            mode: &ctx.mode,
            plan: ctx.plan.as_ref(),
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
        assert!(registered.is_visible_in_read_only());
        assert!(!registered.is_safe_in_read_only());
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
        assert_eq!(registered.tool.audience(), ToolAudience::all());
    }

    /// Dispatch hands a read to the plan's own approval rather than ordinary
    /// policy, so it completes with no one there to answer.
    #[test_case(DefaultEffect::Prompt; "default_prompt")]
    #[test_case(DefaultEffect::Deny; "default_deny")]
    fn a_dispatched_read_needs_no_answer(default: DefaultEffect) {
        for remote_target in [false, true] {
            let (root, mut ctx) = if remote_target {
                remote()
            } else {
                let (root, ctx, _) = local();
                (root, ctx)
            };
            crate::tools::native::register(&ctx.registry, FeatureFlags::all()).unwrap();
            run(&ctx, json!({"action": "write", "content": CONTENT}))
                .output
                .unwrap();
            ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    default,
                    ..Default::default()
                },
                root.path().to_path_buf(),
                Arc::default(),
            ));
            let done = smol::block_on(tool_dispatch::run(
                &ctx.registry,
                None,
                NAME.into(),
                NAME,
                &json!({"action": PlanAccess::Read.operation()}),
                &ctx,
                Emit::Silent,
            ));
            assert!(!done.is_error, "{}", done.output.as_text());
            assert_eq!(done.output.as_text(), CONTENT);
        }
    }

    fn prompted(events: &flume::Receiver<Envelope>) -> bool {
        events
            .drain()
            .any(|envelope| matches!(envelope.event, AgentEvent::PermissionRequest(_)))
    }

    /// The main agent's Build write takes the exact-target approval a planning
    /// write takes: a rule or a default Deny still governs it, an ask needs
    /// someone to answer, and nothing else asks.
    #[test_case(DefaultEffect::Prompt, None, Approval::Automatic; "default_prompt")]
    #[test_case(DefaultEffect::Allow, None, Approval::Automatic; "default_allow")]
    #[test_case(DefaultEffect::Deny, None, Approval::Refused; "default_deny")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Deny), Approval::Refused; "deny_rule")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Ask), Approval::Asked; "ask_rule")]
    fn a_dispatched_build_write_follows_the_exact_target_approval(
        default: DefaultEffect,
        rule: Option<Effect>,
        approval: Approval,
    ) {
        for (session, answering) in [
            (Session::Build, false),
            (Session::Build, true),
            (Session::RemoteBuild, false),
            (Session::RemoteBuild, true),
        ] {
            let ((root, owner), mut ctx) = session_context(session, ToolAudience::MAIN);
            crate::tools::native::register(&ctx.registry, FeatureFlags::all()).unwrap();
            ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    default,
                    rules: rule
                        .into_iter()
                        .map(|effect| PermissionRule {
                            tool: ToolKey::native(NAME),
                            scope: None,
                            effect,
                        })
                        .collect(),
                    ..Default::default()
                },
                root.path().to_path_buf(),
                Arc::default(),
            ));
            let (sender, events) = flume::unbounded();
            ctx.event_tx = EventSender::new(sender, 0);
            let (_answers, responses) = flume::unbounded();
            ctx.user_response_rx = answering.then(|| Arc::new(async_lock::Mutex::new(responses)));
            let write = json!({"action": PlanAccess::Write.operation(), "content": UPDATED});
            let mut dispatch = Box::pin(tool_dispatch::run(
                &ctx.registry,
                None,
                NAME.into(),
                NAME,
                &write,
                &ctx,
                Emit::Silent,
            ));
            let done = smol::block_on(async {
                if approval == Approval::Asked && answering {
                    assert!(poll_once(&mut dispatch).await.is_none());
                    assert!(prompted(&events));
                    assert!(ctx.permissions.answer(NAME, PermissionAnswer::Deny));
                }
                dispatch.await
            });
            assert!(!prompted(&events));
            let stored = run(&owner, json!({"action": "read"}))
                .output
                .unwrap()
                .as_text();
            if approval == Approval::Automatic {
                assert!(!done.is_error, "{}", done.output.as_text());
                assert_eq!(stored, UPDATED);
            } else {
                assert!(
                    done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                    "{}",
                    done.output.as_text()
                );
                assert_eq!(stored, CONTENT);
            }
        }
    }

    /// Refused ahead of the handler and every permission step, so even a
    /// default that denies everything never gets to answer.
    #[test_case(Session::Unbound, ToolAudience::MAIN, TOOL_DISABLED_SUFFIX; "unbound_main")]
    #[test_case(Session::Build, ToolAudience::GENERAL_SUB, WRITE_DENIED; "build_task")]
    #[test_case(Session::RemoteBuild, ToolAudience::RESEARCH_SUB, WRITE_DENIED; "remote_build_task")]
    #[test_case(Session::ReadOnly, ToolAudience::GENERAL_SUB, WRITE_DENIED; "read_only_task")]
    fn a_dispatched_write_without_authority_is_refused_before_permissions(
        session: Session,
        audience: ToolAudience,
        refusal: &str,
    ) {
        let ((root, owner), mut ctx) = session_context(session, audience);
        crate::tools::native::register(&ctx.registry, FeatureFlags::all()).unwrap();
        ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                ..Default::default()
            },
            root.path().to_path_buf(),
            Arc::default(),
        ));
        let done = smol::block_on(tool_dispatch::run(
            &ctx.registry,
            None,
            NAME.into(),
            NAME,
            &json!({"action": PlanAccess::Write.operation(), "content": UPDATED}),
            &ctx,
            Emit::Silent,
        ));
        assert!(done.is_error);
        assert!(
            done.output.as_text().ends_with(refusal),
            "{}",
            done.output.as_text()
        );
        assert_eq!(
            run(&owner, json!({"action": "read"}))
                .output
                .unwrap()
                .as_text(),
            CONTENT
        );
    }

    /// Claims the plan's write authority while naming more than its target.
    #[derive(Clone)]
    struct PlanClaim(Vec<PathBuf>);

    impl Tool for PlanClaim {
        fn name(&self) -> &str {
            CLAIM_NAME
        }

        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            CLAIM_NAME.into()
        }

        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }

        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(self.clone()))
        }
    }

    impl ToolInvocation for PlanClaim {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(CLAIM_NAME.into()))
        }

        fn active_plan_access(&self) -> Option<PlanAccess> {
            Some(PlanAccess::Write)
        }

        fn mutation_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
            self.0.clone()
        }

        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async {
                ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain(CLAIM_NAME.into())))
            })
        }
    }

    /// With a project root that cannot be resolved every write is
    /// unverifiable, so only the target the plan authority verified may still
    /// be written: the Build write to the bound plan lands, and a call claiming
    /// the plan's authority for a second file is stopped at that file.
    #[test]
    fn the_boundary_skip_covers_only_the_verified_target() {
        let ((root, owner), mut ctx) = session_context(Session::Build, ToolAudience::MAIN);
        let other = root.path().join(OTHER_FILE_NAME);
        crate::tools::native::register(&ctx.registry, FeatureFlags::all()).unwrap();
        ctx.registry
            .register_audited(
                Arc::new(PlanClaim(vec![root.path().join(FILE_NAME), other.clone()])),
                NamedMock::source(),
                ToolEffect::Mutating,
            )
            .unwrap();
        ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            PathBuf::new(),
            Arc::default(),
        ));
        let dispatch = |name: &str, input: Value| {
            smol::block_on(tool_dispatch::run(
                &ctx.registry,
                None,
                name.into(),
                name,
                &input,
                &ctx,
                Emit::Silent,
            ))
        };

        let written = dispatch(
            NAME,
            json!({"action": PlanAccess::Write.operation(), "content": UPDATED}),
        );
        assert!(!written.is_error, "{}", written.output.as_text());
        assert_eq!(
            run(&owner, json!({"action": "read"}))
                .output
                .unwrap()
                .as_text(),
            UPDATED
        );
        let claimed = dispatch(CLAIM_NAME, json!({})).output.as_text();
        assert!(
            claimed.starts_with(BOUNDARY_UNVERIFIABLE_PREFIX)
                && claimed.contains(other.to_str().unwrap()),
            "{claimed}"
        );
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

    /// A context in `session` for `audience`, beside the main planning context
    /// that owns the plan and has already saved `CONTENT` to it.
    fn session_context(
        session: Session,
        audience: ToolAudience,
    ) -> ((TempDir, ToolContext), ToolContext) {
        let owner = match session {
            Session::RemotePlan | Session::RemoteBuild => remote(),
            Session::Plan | Session::Build | Session::Unbound | Session::ReadOnly => {
                let (root, ctx, _) = local();
                (root, ctx)
            }
        };
        run(&owner.1, json!({"action":"write", "content":CONTENT}))
            .output
            .unwrap();
        let mut ctx = owner.1.clone();
        let (mode, plan) = match session {
            Session::Plan | Session::RemotePlan => (ctx.mode.clone(), None),
            Session::Build | Session::RemoteBuild => (AgentMode::Build, ctx.session_plan()),
            Session::Unbound => (AgentMode::Build, None),
            Session::ReadOnly => (AgentMode::ReadOnly, ctx.session_plan()),
        };
        ctx.mode = mode;
        ctx.plan = plan;
        ctx.audience = audience;
        (owner, ctx)
    }

    #[test_case(Session::Plan, ToolAudience::MAIN, None, None; "plan_main")]
    #[test_case(Session::Plan, ToolAudience::RESEARCH_SUB, None, Some(WRITE_DENIED); "plan_research")]
    #[test_case(Session::Plan, ToolAudience::GENERAL_SUB, None, Some(WRITE_DENIED); "plan_general")]
    #[test_case(Session::RemotePlan, ToolAudience::MAIN, None, None; "remote_plan_main")]
    #[test_case(Session::RemotePlan, ToolAudience::RESEARCH_SUB, None, Some(WRITE_DENIED); "remote_plan_research")]
    #[test_case(Session::RemotePlan, ToolAudience::GENERAL_SUB, None, Some(WRITE_DENIED); "remote_plan_general")]
    #[test_case(Session::Build, ToolAudience::MAIN, None, None; "build_main")]
    #[test_case(Session::Build, ToolAudience::RESEARCH_SUB, None, Some(WRITE_DENIED); "build_research")]
    #[test_case(Session::Build, ToolAudience::GENERAL_SUB, None, Some(WRITE_DENIED); "build_general")]
    #[test_case(Session::RemoteBuild, ToolAudience::MAIN, None, None; "remote_build_main")]
    #[test_case(Session::RemoteBuild, ToolAudience::RESEARCH_SUB, None, Some(WRITE_DENIED); "remote_build_research")]
    #[test_case(Session::RemoteBuild, ToolAudience::GENERAL_SUB, None, Some(WRITE_DENIED); "remote_build_general")]
    #[test_case(Session::Unbound, ToolAudience::MAIN, Some(NO_TARGET), Some(NO_TARGET); "unbound_main")]
    #[test_case(Session::Unbound, ToolAudience::RESEARCH_SUB, Some(NO_TARGET), Some(WRITE_DENIED); "unbound_research")]
    #[test_case(Session::Unbound, ToolAudience::GENERAL_SUB, Some(NO_TARGET), Some(WRITE_DENIED); "unbound_general")]
    #[test_case(Session::ReadOnly, ToolAudience::MAIN, None, Some(WRITE_DENIED); "read_only_main")]
    #[test_case(Session::ReadOnly, ToolAudience::RESEARCH_SUB, None, Some(WRITE_DENIED); "read_only_research")]
    #[test_case(Session::ReadOnly, ToolAudience::GENERAL_SUB, None, Some(WRITE_DENIED); "read_only_general")]
    fn session_plan_access_follows_audience_and_mode(
        session: Session,
        audience: ToolAudience,
        read_refusal: Option<&str>,
        write_refusal: Option<&str>,
    ) {
        let ((root, owner), ctx) = session_context(session, audience);
        let read = json!({"action": PlanAccess::Read.operation()});
        let write = json!({"action": PlanAccess::Write.operation(), "content": UPDATED});
        for (input, refusal) in [(&read, read_refusal), (&write, write_refusal)] {
            let analyzed = authority(&ctx, &host_cwd(&ctx)).analyze(input);
            let call = PlanTool.parse(input).unwrap();
            let preflight = smol::block_on(call.preflight(&ctx));
            match refusal {
                None => assert_eq!(
                    analyzed.unwrap().resources,
                    preflight.unwrap().unwrap().resources
                ),
                Some(message) => {
                    assert_eq!(analyzed.unwrap_err().message, message);
                    let preflight = preflight.unwrap_err();
                    assert_eq!(preflight.failure, ToolFailure::Denied);
                    assert_eq!(preflight.message, message);
                }
            }
        }
        assert_eq!(
            write_target(&ctx).err().map(|error| error.message),
            write_refusal.map(str::to_owned)
        );
        if write_refusal.is_some() {
            let call = PlanTool.parse(&write).unwrap();
            assert!(call.mutation_targets(&ctx).is_empty());
            assert!(call.record_scope(&ctx, root.path()).is_none());
        }

        let result = run(&ctx, read.clone());
        match read_refusal {
            None => assert_eq!(result.output.unwrap().as_text(), CONTENT),
            Some(message) => {
                assert_eq!(result.failure, Some(ToolFailure::Denied));
                assert_eq!(result.output.unwrap_err(), message);
            }
        }
        let result = run(&ctx, write);
        let stored = run(&owner, read).output.unwrap().as_text();
        match write_refusal {
            None => {
                assert!(!result.is_error);
                assert_eq!(stored, UPDATED);
            }
            Some(message) => {
                assert_eq!(result.failure, Some(ToolFailure::Denied));
                assert_eq!(result.output.unwrap_err(), message);
                assert_eq!(stored, CONTENT);
            }
        }
    }

    #[test]
    fn a_planning_target_wins_over_a_mismatched_binding() {
        let (root, mut ctx, path) = local();
        ctx.plan = Some(PlanTarget::Local(root.path().join(OTHER_FILE_NAME)));
        assert_eq!(
            verified_target(&ctx).unwrap(),
            PlanTarget::Local(path.clone())
        );
        assert_eq!(write_target(&ctx).unwrap(), PlanTarget::Local(path.clone()));
        run(&ctx, json!({"action":"write", "content":CONTENT}))
            .output
            .unwrap();
        assert_eq!(PlanFile::new(path).unwrap().read().unwrap(), CONTENT);
        assert!(!root.path().join(OTHER_FILE_NAME).exists());
    }

    #[test_case(AgentMode::Build, ToolAudience::MAIN; "build")]
    #[test_case(AgentMode::ReadOnly, ToolAudience::GENERAL_SUB; "task")]
    fn a_bound_local_target_outside_the_plans_dir_and_cwd_is_refused(
        mode: AgentMode,
        audience: ToolAudience,
    ) {
        let (_root, mut ctx, _) = local();
        ctx.mode = mode;
        ctx.audience = audience;
        ctx.plan = Some(PlanTarget::Local(PathBuf::from(ABSOLUTE_PLAN)));
        let read = json!({"action":"read"});
        let preflight = smol::block_on(PlanTool.parse(&read).unwrap().preflight(&ctx)).unwrap_err();
        assert_eq!(preflight.failure, ToolFailure::Denied);
        assert_eq!(preflight.message, INVALID_TARGET);
        assert!(PlanTool.parse(&read).unwrap().read_targets(&ctx).is_empty());
        assert_eq!(run(&ctx, read).output.unwrap_err(), INVALID_TARGET);
    }

    #[test_case(false; "owning_session")]
    #[test_case(true; "foreign_session")]
    fn a_task_reads_a_remote_plan_as_its_parent_session(foreign: bool) {
        let ((_root, owner), mut ctx) =
            session_context(Session::RemotePlan, ToolAudience::GENERAL_SUB);
        ctx.plan = owner.session_plan();
        ctx.mode = AgentMode::ReadOnly;
        if foreign {
            ctx.session_id = Some(SessionRef::generate());
        }
        let result = run(&ctx, json!({"action":"read"}));
        if foreign {
            assert_eq!(result.failure, Some(ToolFailure::Denied));
        } else {
            assert_eq!(result.output.unwrap().as_text(), CONTENT);
        }
    }

    /// Without a document store a read fails on the store, while a task's
    /// write is refused before it gets that far.
    #[test]
    fn a_task_write_is_refused_before_storage() {
        let (_root, mut ctx) = remote();
        ctx.audience = ToolAudience::GENERAL_SUB;
        ctx.local_documents = None;
        for (input, message) in [
            (json!({"action":"read"}), STORE_UNAVAILABLE),
            (json!({"action":"write", "content":CONTENT}), WRITE_DENIED),
        ] {
            let preflight =
                smol::block_on(PlanTool.parse(&input).unwrap().preflight(&ctx)).unwrap_err();
            assert_eq!(preflight.message, message);
            assert_eq!(run(&ctx, input).output.unwrap_err(), message);
        }
    }

    #[test_case(false, false; "absolute")]
    #[test_case(true, false; "legacy_relative")]
    #[test_case(false, true; "build_binding")]
    fn local_round_trip_retains_committed_content_and_exact_record_scope(
        relative: bool,
        bound: bool,
    ) {
        let (root, mut ctx, path) = local();
        if relative {
            ctx.mode = AgentMode::Plan(PathBuf::from(FILE_NAME));
        }
        if bound {
            ctx.plan = ctx.mode.plan_target();
            ctx.mode = AgentMode::Build;
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
        assert_eq!(write.active_plan_access(), Some(PlanAccess::Write));
        assert_eq!(read.active_plan_access(), Some(PlanAccess::Read));
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
        assert!(result.annotation.is_none());
        let output = result.output.unwrap();
        assert_eq!(output.as_text(), CONTENT);
        let persisted = serde_json::to_string(&output).unwrap();
        let restored: ToolOutput = serde_json::from_str(&persisted).unwrap();
        let saved = restored.plan_write_result().unwrap();
        assert_eq!(saved.target(), &PlanTarget::Local(path.clone()));
        assert_eq!(saved.content(), CONTENT);
        assert_eq!(
            PlanFile::new(path.clone()).unwrap().read().unwrap(),
            CONTENT
        );
        let update = run(&ctx, json!({"action":"write", "content":UPDATED}));
        let updated = update.output.unwrap().plan_write_result().unwrap();
        assert_eq!(updated.content(), UPDATED);
        let reread = smol::block_on(read.execute(&ctx));
        assert!(reread.annotation.is_none());
        assert_eq!(reread.output.unwrap().as_text(), UPDATED);
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
        assert!(result.annotation.is_none());
        assert_eq!(result.model_output.as_deref(), Some(WRITE_RECEIPT));
        let output = result.output.unwrap();
        let restored: ToolOutput =
            serde_json::from_value(serde_json::to_value(&output).unwrap()).unwrap();
        let saved = restored.plan_write_result().unwrap();
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
        assert_eq!(document.content, CONTENT);
        let read = run(&ctx, json!({"action":"read"}));
        assert!(read.annotation.is_none());
        assert_eq!(read.output.unwrap().as_text(), CONTENT);
    }

    #[test_case(PlanTarget::Local(PathBuf::from(ABSOLUTE_PLAN)); "local")]
    #[test_case(PlanTarget::Remote(PlanRef::new(REMOTE_PLAN).unwrap()); "remote")]
    fn write_result_round_trips_without_a_revision(target: PlanTarget) {
        let written = PlanWriteResult::new(target, CONTENT.to_owned());
        let annotation = written.annotation().unwrap();
        let encoded: Value =
            serde_json::from_str(annotation.strip_prefix(WRITE_RESULT_PREFIX).unwrap()).unwrap();
        let fields: BTreeSet<&str> = encoded
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, BTreeSet::from(ENCODED_FIELDS));
        assert_eq!(parse_write_result(&annotation), Some(written));
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

    #[test_case(false; "plan_mode")]
    #[test_case(true; "build_binding")]
    fn owned_local_state_never_records_a_workspace(bound: bool) {
        let (root, mut ctx, _) = local();
        let state = StateDir::resolve_without_create().unwrap();
        let path = state
            .persistent_path()
            .join(project_subdir(root.path()))
            .join(PLANS_DIR)
            .join(FILE_NAME);
        if bound {
            ctx.mode = AgentMode::Build;
            ctx.plan = Some(PlanTarget::Local(path.clone()));
        } else {
            ctx.mode = AgentMode::Plan(path.clone());
        }
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
    fn large_plan_results_reach_the_model_without_state_or_annotations(remote_target: bool) {
        let (_root, mut ctx) = if remote_target {
            remote()
        } else {
            let (root, ctx, _) = local();
            (root, ctx)
        };
        let content = CONTENT.repeat(LARGE_CONTENT_REPEATS);
        let result = run(&ctx, json!({"action":"write", "content":content}));
        assert_eq!(result.model_output.as_deref(), Some(WRITE_RECEIPT));
        assert!(result.annotation.is_none());
        let output = result.output.unwrap();
        let saved = output.plan_write_result().unwrap();
        assert_eq!(saved.content(), content);
        let read = run(&ctx, json!({"action":"read"}));
        assert_eq!(read.output.unwrap().as_text(), content);
        assert!(read.model_output.is_none());
        assert!(read.annotation.is_none());

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
        assert!(entry.annotation.is_none());
        assert!(text.contains(&format!("{NAME}\n{WRITE_RECEIPT}\n\n")));
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
        assert!(live.iter().all(|entry| entry.annotation.is_none()));
    }
}
