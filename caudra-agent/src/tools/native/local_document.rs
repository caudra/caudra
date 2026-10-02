use std::borrow::Cow;
use std::collections::BTreeMap;

use caudra_storage::local_documents::{
    DocumentRevision, LocalDocumentError, LocalDocumentStore, PatchEdit,
};
use caudra_workspace::{LocalDocumentRef, MemoryRef, PlanRef};
use serde_json::{Value, json};

use crate::permissions::{PermissionResource, PermissionResourceKind, PermissionRisk};
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent, PermissionScopes,
    PlanModeAccess, Tool, ToolError, ToolExecResult, ToolFailure, ToolInvocation,
};
use crate::tools::{
    DescriptionContext, LOCAL_DOCUMENT_APPLY_PATCH_TOOL_NAME, LOCAL_DOCUMENT_READ_TOOL_NAME,
    LOCAL_DOCUMENT_WRITE_TOOL_NAME, ToolContext,
};
use crate::types::ToolOutput;

pub const READ_DESCRIPTION: &str = "Read a client-owned plan or memory document by opaque reference. Available only for remote workspace sessions; it never accepts or reveals a host path.";
pub const WRITE_DESCRIPTION: &str = "Replace a client-owned plan or memory document by opaque reference. Available only for remote workspace sessions; it never accepts an arbitrary path.";
pub const PATCH_DESCRIPTION: &str = "Apply exact text replacements to a client-owned plan or memory document by opaque reference. Requires the revision returned by the latest read and rejects stale edits.";

const WRITE_RECEIPT: &str = "local document written";
const PATCH_RECEIPT: &str = "local document patched";

pub struct LocalDocumentRead;
pub struct LocalDocumentWrite;
pub struct LocalDocumentApplyPatch;

impl Tool for LocalDocumentRead {
    fn name(&self) -> &str {
        LOCAL_DOCUMENT_READ_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(READ_DESCRIPTION)
    }

    fn schema(&self) -> Value {
        reference_schema(&[])
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        Ok(Box::new(LocalDocumentCall {
            operation: Operation::Read,
            reference: parse_reference(input)?,
        }))
    }
}

impl Tool for LocalDocumentWrite {
    fn name(&self) -> &str {
        LOCAL_DOCUMENT_WRITE_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(WRITE_DESCRIPTION)
    }

    fn schema(&self) -> Value {
        reference_schema(&[(
            "content",
            json!({
                "type": "string",
                "description": "Complete replacement content for the document."
            }),
        )])
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let content = required_string(input, "content")?.to_owned();
        Ok(Box::new(LocalDocumentCall {
            operation: Operation::Write { content },
            reference: parse_reference(input)?,
        }))
    }
}

impl Tool for LocalDocumentApplyPatch {
    fn name(&self) -> &str {
        LOCAL_DOCUMENT_APPLY_PATCH_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(PATCH_DESCRIPTION)
    }

    fn schema(&self) -> Value {
        reference_schema(&[
            (
                "revision",
                json!({
                    "type": "string",
                    "description": "Revision returned by the latest read."
                }),
            ),
            (
                "edits",
                json!({
                    "type": "array",
                    "description": "Exact text replacements applied in order.",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": {
                                "type": "string",
                                "description": "Text that must occur exactly once."
                            },
                            "new": {
                                "type": "string",
                                "description": "Replacement text, which may be empty."
                            }
                        },
                        "required": ["old", "new"],
                        "additionalProperties": false
                    }
                }),
            ),
        ])
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let revision = DocumentRevision::new(required_string(input, "revision")?)
            .map_err(|error| ParseError::custom(error.to_string()))?;
        let edits = input
            .get("edits")
            .and_then(Value::as_array)
            .filter(|edits| !edits.is_empty())
            .ok_or_else(|| ParseError::custom("edits must be a non-empty array"))?
            .iter()
            .map(|edit| {
                Ok(PatchEdit {
                    old: required_string(edit, "old")?.to_owned(),
                    new: required_string_allow_empty(edit, "new")?.to_owned(),
                })
            })
            .collect::<Result<Vec<_>, ParseError>>()?;
        Ok(Box::new(LocalDocumentCall {
            operation: Operation::Patch { revision, edits },
            reference: parse_reference(input)?,
        }))
    }
}

enum Operation {
    Read,
    Write {
        content: String,
    },
    Patch {
        revision: DocumentRevision,
        edits: Vec<PatchEdit>,
    },
}

struct LocalDocumentCall {
    operation: Operation,
    reference: LocalDocumentRef,
}

impl ToolInvocation for LocalDocumentCall {
    /// The document, not what is being done to it: each operation is its own
    /// tool, so the row's own label already carries the verb and inflects it.
    /// Naming only the document is also what the arguments can say mid-stream,
    /// which is what keeps the streaming row and the settled one the same row.
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(reference_label(&self.reference)))
    }

    fn local_document_target(&self) -> Option<&LocalDocumentRef> {
        (!matches!(self.operation, Operation::Read)).then_some(&self.reference)
    }

    fn plan_mode_access(&self) -> PlanModeAccess {
        if matches!(self.operation, Operation::Read) {
            PlanModeAccess::Standard
        } else {
            PlanModeAccess::Prompted
        }
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> crate::tools::registry::BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async move {
            scoped_store(ctx)?;
            if matches!(self.operation, Operation::Read) {
                return Ok(None);
            }
            Ok(Some(PermissionIntent::new(
                PermissionScopes::single(format!(
                    "local-document:{}:{}",
                    reference_kind(&self.reference),
                    reference_id(&self.reference)
                )),
                vec![PermissionResource {
                    kind: PermissionResourceKind::Custom {
                        name: "local_document".to_owned(),
                    },
                    value: format!(
                        "{}:{}",
                        reference_kind(&self.reference),
                        reference_id(&self.reference)
                    ),
                    access: None,
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::new(),
                }],
                PermissionRisk::Low,
            )))
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let result = execute(&self, ctx);
            match result {
                Ok((output, revision)) => {
                    // A write's reply is a receipt. The document is what the
                    // reader came for, and the model already has it, so the
                    // card keeps the document and only the receipt goes back.
                    let written = match &self.operation {
                        Operation::Write { content } if !content.trim().is_empty() => Some(content),
                        _ => None,
                    };
                    let mut result = match written {
                        Some(content) => {
                            ToolExecResult::from(Ok(ToolOutput::Markdown(content.as_str().into())))
                                .with_model_output(Some(output))
                        }
                        None => ToolExecResult::from(Ok(ToolOutput::Plain(output.into()))),
                    };
                    result.annotation = Some(if matches!(self.operation, Operation::Read) {
                        format!("revision {}", revision.as_str())
                    } else {
                        format!(
                            "local_document:{}:{};revision:{}",
                            reference_kind(&self.reference),
                            reference_id(&self.reference),
                            revision.as_str()
                        )
                    });
                    result
                }
                Err(error) => ToolExecResult::failed(error.failure, error.message),
            }
        })
    }
}

/// A reference the store cannot place is refused as not yours rather than as
/// missing, so that refusal stays [`ToolFailure::Other`].
impl From<LocalDocumentError> for ToolError {
    fn from(error: LocalDocumentError) -> Self {
        let failure = match &error {
            LocalDocumentError::InvalidReference
            | LocalDocumentError::InvalidMemoryName
            | LocalDocumentError::TooLarge
            | LocalDocumentError::PatchConflict => ToolFailure::InvalidInput,
            LocalDocumentError::Symlink => ToolFailure::Denied,
            LocalDocumentError::Io(error) => ToolFailure::from(error),
            _ => ToolFailure::Other,
        };
        Self::new(failure, error.to_string())
    }
}

pub(super) fn scoped_store(ctx: &ToolContext) -> Result<&LocalDocumentStore, ToolError> {
    let workspace = ctx
        .workspace_session
        .as_ref()
        .ok_or_else(|| "local document tools require a remote workspace session".to_owned())?;
    let store = ctx
        .local_documents
        .as_ref()
        .ok_or_else(|| "local document store is unavailable".to_owned())?;
    store.validate_binding(workspace.binding())?;
    Ok(store)
}

fn execute(
    call: &LocalDocumentCall,
    ctx: &ToolContext,
) -> Result<(String, DocumentRevision), ToolError> {
    let store = scoped_store(ctx)?;
    let project = store.project_key();
    let session_id = ctx.session_id.as_ref().map(|session| session.as_str());
    match &call.operation {
        Operation::Read => {
            let document = store.read(project, session_id, &call.reference)?;
            Ok((
                format!(
                    "kind: {}\nreference: {}\nrevision: {}\n\n{}",
                    reference_kind(&call.reference),
                    reference_id(&call.reference),
                    document.revision.as_str(),
                    document.content
                ),
                document.revision,
            ))
        }
        Operation::Write { content } => {
            let revision = store.write(project, session_id, &call.reference, content)?;
            Ok((WRITE_RECEIPT.to_owned(), revision))
        }
        Operation::Patch { revision, edits } => {
            let revision =
                store.apply_patch(project, session_id, &call.reference, revision, edits)?;
            Ok((PATCH_RECEIPT.to_owned(), revision))
        }
    }
}

fn reference_schema(extra: &[(&str, Value)]) -> Value {
    let mut properties = serde_json::Map::from_iter([
        (
            "kind".to_owned(),
            json!({
                "type": "string",
                "enum": ["plan", "memory"],
                "description": "Document kind associated with the reference."
            }),
        ),
        (
            "reference".to_owned(),
            json!({
                "type": "string",
                "description": "Opaque reference supplied by Caudra or a previous tool result."
            }),
        ),
    ]);
    let mut required = vec![
        Value::String("kind".to_owned()),
        Value::String("reference".to_owned()),
    ];
    for (name, schema) in extra {
        properties.insert((*name).to_owned(), schema.clone());
        required.push(Value::String((*name).to_owned()));
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn parse_reference(input: &Value) -> Result<LocalDocumentRef, ParseError> {
    let kind = required_string(input, "kind")?;
    let reference = required_string(input, "reference")?.to_owned();
    match kind {
        "plan" => PlanRef::new(reference)
            .map(LocalDocumentRef::Plan)
            .map_err(|error| ParseError::custom(error.to_string())),
        "memory" => MemoryRef::new(reference)
            .map(LocalDocumentRef::Memory)
            .map_err(|error| ParseError::custom(error.to_string())),
        _ => Err(ParseError::custom("kind must be 'plan' or 'memory'")),
    }
}

fn required_string<'a>(input: &'a Value, name: &str) -> Result<&'a str, ParseError> {
    required_string_allow_empty(input, name).and_then(|value| {
        (!value.is_empty())
            .then_some(value)
            .ok_or_else(|| ParseError::custom(format!("{name} must not be empty")))
    })
}

fn required_string_allow_empty<'a>(input: &'a Value, name: &str) -> Result<&'a str, ParseError> {
    input
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::custom(format!("{name} must be a string")))
}

fn reference_kind(reference: &LocalDocumentRef) -> &'static str {
    match reference {
        LocalDocumentRef::Plan(_) => "plan",
        LocalDocumentRef::Memory(_) => "memory",
    }
}

fn reference_id(reference: &LocalDocumentRef) -> &str {
    match reference {
        LocalDocumentRef::Plan(reference) => reference.as_str(),
        LocalDocumentRef::Memory(reference) => reference.as_str(),
    }
}

fn reference_label(reference: &LocalDocumentRef) -> String {
    format!("{} {}", reference_kind(reference), reference_id(reference))
}

#[cfg(test)]
pub(super) mod tests {
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use caudra_config::FeatureFlags;
    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use caudra_storage::local_documents::LocalDocumentStore;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceCapabilities, WorkspaceCursor, WorkspaceHandle, WorkspaceServices,
        WorkspaceSession,
    };
    use serde_json::json;
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::agent::tool_dispatch::{Emit, run};
    use crate::tools::ToolRegistry;
    use crate::tools::native::memory::MemoryTool;
    use crate::tools::test_support::stub_ctx;

    const PROJECT: &str = "project-a";
    const PLAN: &str = "finished plan";
    const WRONG_OWNER: &str = "local document does not belong to this project or session";
    const RENDERED: &str = "the reader sees the document, the model sees the receipt";
    const HOST_PATH_NAMED: &str = "a remote note has no host path to report";
    const REMOTE_WRITE_FAILED: &str = "a remote note is written to the client's store";
    #[cfg(unix)]
    const DIRECTORY_MODE: u32 = 0o700;

    pub(in crate::tools::native) fn tempdir() -> TempDir {
        let mut builder = Builder::new();
        #[cfg(unix)]
        builder.permissions(Permissions::from_mode(DIRECTORY_MODE));
        builder
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap()
    }

    fn workspace() -> WorkspaceSession {
        workspace_for_principal("principal")
    }

    pub(in crate::tools::native) fn workspace_for_principal(subject: &str) -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").expect("trust anchor"),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .expect("authority");
        let principal =
            AuthenticatedPrincipalId::new(authority.clone(), subject).expect("principal");
        let project = ProjectIdentity::new(
            authority.clone(),
            ProjectKey::new(PROJECT).expect("project key"),
        );
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("binding").expect("binding id"),
            authority.clone(),
            principal,
            project,
        )
        .expect("binding");
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").expect("resource id")),
            1,
            CwdHandle::new("cwd").expect("cwd handle"),
        );
        let handle = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::new([]),
            WorkspaceServices::default(),
        )
        .expect("workspace handle");
        WorkspaceSession::new(handle, binding, cursor).expect("workspace session")
    }

    /// A remote session with an empty plan in it, which every document call
    /// needs before it can say anything at all.
    struct Remote {
        root: tempfile::TempDir,
        registry: Arc<ToolRegistry>,
        ctx: crate::tools::ToolContext,
        store: Arc<LocalDocumentStore>,
        plan: PlanRef,
    }

    fn remote() -> Remote {
        let root = tempdir();
        let workspace = workspace();
        let session_id = SessionRef::generate();
        let store = Arc::new(LocalDocumentStore::remote(
            StateDir::from_path(root.path().join("state")),
            workspace.binding(),
        ));
        let plan = store
            .create_plan(workspace.binding().project().key(), session_id.as_str())
            .expect("create plan");
        let registry = Arc::new(ToolRegistry::new());
        crate::tools::native::register(&registry, FeatureFlags::all())
            .expect("register native tools");
        let mut ctx = stub_ctx(&AgentMode::RemotePlan(plan.clone()));
        ctx.registry = Arc::clone(&registry);
        ctx.session_id = Some(session_id);
        ctx.workspace_session = Some(workspace);
        ctx.local_documents = Some(Arc::clone(&store));
        Remote {
            root,
            registry,
            ctx,
            store,
            plan,
        }
    }

    #[test]
    fn remote_plan_write_is_typed_and_completable_without_a_host_path() {
        smol::block_on(async {
            let Remote {
                root,
                registry,
                ctx,
                store,
                plan,
            } = remote();

            let done = run(
                &registry,
                None,
                "call".into(),
                LOCAL_DOCUMENT_WRITE_TOOL_NAME,
                &json!({"kind": "plan", "reference": plan.as_str(), "content": PLAN}),
                &ctx,
                Emit::Silent,
            )
            .await;
            let reference = LocalDocumentRef::Plan(plan);

            assert!(!done.is_error, "{}", done.output.as_text());
            assert!(done.wrote_document(&reference));
            assert!(done.written_paths().next().is_none());
            assert_eq!(
                store
                    .read(
                        store.project_key(),
                        ctx.session_id.as_ref().map(SessionRef::as_str),
                        &reference,
                    )
                    .expect("read plan")
                    .content,
                PLAN
            );

            let read = run(
                &registry,
                None,
                "read".into(),
                LOCAL_DOCUMENT_READ_TOOL_NAME,
                &json!({"kind": "plan", "reference": reference_id(&reference)}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!read.is_error);
            assert!(read.output.as_text().contains(PLAN));
            assert!(
                !read
                    .output
                    .as_text()
                    .contains(&root.path().to_string_lossy().into_owned())
            );

            let revision = store
                .read(
                    store.project_key(),
                    ctx.session_id.as_ref().map(SessionRef::as_str),
                    &reference,
                )
                .expect("read revision")
                .revision;
            let patched = run(
                &registry,
                None,
                "patch".into(),
                LOCAL_DOCUMENT_APPLY_PATCH_TOOL_NAME,
                &json!({
                    "kind": "plan",
                    "reference": reference_id(&reference),
                    "revision": revision.as_str(),
                    "edits": [{"old": "finished", "new": "approved"}]
                }),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!patched.is_error, "{}", patched.output.as_text());
            assert!(patched.wrote_document(&reference));
            assert_eq!(
                store
                    .read(
                        store.project_key(),
                        ctx.session_id.as_ref().map(SessionRef::as_str),
                        &reference,
                    )
                    .expect("read patched plan")
                    .content,
                "approved plan"
            );
        });
    }

    /// A write's reply is a receipt, so rendering it renders nothing. The
    /// document is what the reader came for, and the model wrote it and does
    /// not need it back.
    #[test]
    fn a_document_write_renders_the_document_and_replies_with_the_receipt() {
        smol::block_on(async {
            // Held whole: the struct owns the temporary directory every call
            // in it reads from.
            let remote = remote();
            let plan = remote.plan.clone();
            let done = run(
                &remote.registry,
                None,
                "call".into(),
                LOCAL_DOCUMENT_WRITE_TOOL_NAME,
                &json!({"kind": "plan", "reference": plan.as_str(), "content": PLAN}),
                &remote.ctx,
                Emit::Silent,
            )
            .await;
            assert!(!done.is_error, "{}", done.output.as_text());
            assert!(matches!(done.output, ToolOutput::Markdown(_)), "{RENDERED}");
            assert_eq!(done.output.as_text(), PLAN, "{RENDERED}");
            assert_eq!(
                done.model_output.as_deref(),
                Some(WRITE_RECEIPT),
                "{RENDERED}"
            );
            // The marker the transcript recognises a plan write by has to
            // survive the change of output type.
            assert!(done.wrote_document(&LocalDocumentRef::Plan(plan)));
        });
    }

    /// Each operation is its own tool, so the row's label carries the verb and
    /// the header carries only what the verb is being applied to.
    #[test]
    fn a_document_header_names_the_document_and_not_the_operation() {
        let plan = PlanRef::new("abc".to_owned()).expect("plan reference");
        let header = LocalDocumentWrite
            .parse(&json!({"kind": "plan", "reference": plan.as_str(), "content": PLAN}))
            .expect("a well-formed write")
            .start_header()
            .into_ready()
            .text();
        assert_eq!(header, format!("plan {}", plan.as_str()));
    }

    #[test_case(LOCAL_DOCUMENT_READ_TOOL_NAME; "document_read")]
    #[test_case(LOCAL_DOCUMENT_WRITE_TOOL_NAME; "document_write")]
    #[test_case("memory"; "memory_list")]
    fn tools_reject_a_store_from_another_principal(tool: &str) {
        smol::block_on(async {
            let root = tempdir();
            let owner = workspace();
            let store = Arc::new(LocalDocumentStore::remote(
                StateDir::from_path(root.path().join("state")),
                owner.binding(),
            ));
            let reference = store
                .write_memory(store.project_key(), "note.md", PLAN)
                .expect("note");
            let registry = Arc::new(ToolRegistry::new());
            crate::tools::native::register(&registry, FeatureFlags::all()).expect("register");
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.registry = Arc::clone(&registry);
            ctx.workspace_session = Some(workspace_for_principal("other"));
            ctx.local_documents = Some(Arc::clone(&store));
            let input = match tool {
                LOCAL_DOCUMENT_READ_TOOL_NAME => {
                    json!({"kind": "memory", "reference": reference.as_str()})
                }
                LOCAL_DOCUMENT_WRITE_TOOL_NAME => {
                    json!({"kind": "memory", "reference": reference.as_str(), "content": "overwrite"})
                }
                _ => json!({"command": "list"}),
            };
            let result = run(
                &registry,
                None,
                "call".into(),
                tool,
                &input,
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(result.is_error);
            assert_eq!(result.output.as_text(), WRONG_OWNER);
            assert_eq!(
                store.list_memories(store.project_key()).expect("list")[0].content,
                PLAN
            );
        });
    }

    /// A remote note lives in the client's store, and the host path a local
    /// write reports would name nothing on it.
    #[test]
    fn a_remote_memory_write_names_no_host_file() {
        let Remote {
            root: _root,
            mut ctx,
            ..
        } = remote();
        ctx.mode = AgentMode::Build;
        let write = MemoryTool
            .parse(&json!({"command": "write", "path": "note.md", "content": PLAN}))
            .expect("a well-formed write");

        let result = smol::block_on(write.execute(&ctx));

        result.output.as_ref().expect(REMOTE_WRITE_FAILED);
        assert_eq!(result.written_path, None, "{HOST_PATH_NAMED}");
    }

    #[test]
    fn a_memory_ref_cannot_be_written_as_the_remote_plan() {
        smol::block_on(async {
            let root = tempdir();
            let workspace = workspace();
            let store = Arc::new(LocalDocumentStore::remote(
                StateDir::from_path(root.path().join("state")),
                workspace.binding(),
            ));
            let session_id = SessionRef::generate();
            let plan = store
                .create_plan(workspace.binding().project().key(), session_id.as_str())
                .expect("create plan");
            let memory = store
                .write_memory(workspace.binding().project().key(), "note.md", "note")
                .expect("write memory");
            let registry = Arc::new(ToolRegistry::new());
            crate::tools::native::register(&registry, FeatureFlags::all())
                .expect("register native tools");
            let mut ctx = stub_ctx(&AgentMode::RemotePlan(plan));
            ctx.registry = Arc::clone(&registry);
            ctx.session_id = Some(session_id);
            ctx.workspace_session = Some(workspace);
            ctx.local_documents = Some(store);

            let done = run(
                &registry,
                None,
                "call".into(),
                LOCAL_DOCUMENT_WRITE_TOOL_NAME,
                &json!({"kind": "memory", "reference": memory.as_str(), "content": "bad"}),
                &ctx,
                Emit::Silent,
            )
            .await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), crate::tools::PLAN_WRITE_RESTRICTED);
        });
    }
}
