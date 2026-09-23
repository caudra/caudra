//! `image_generate`: produce a raster image and write it to disk.
//!
//! Backed today by the ChatGPT subscription path, which is the only backend
//! Caudra can reach without asking the user for a second credential. The tool
//! name and schema are deliberately backend-neutral so another provider can be
//! added without breaking a model-facing contract or existing permission rules.
//!
//! The result is the saved path, not the pixels: a generated image costs vision
//! tokens on every later turn it stays in history, so the model pays that only
//! when it explicitly calls `view_image`.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use async_lock::Mutex;
use caudra_workspace::{
    Mutation, MutationCondition, MutationKind, MutationRequest, OperationState, ResourceId,
    ResourceRevision, WorkspacePath, WorkspaceSession, WriteContent,
};
use serde_json::Value;

use crate::permissions::{PermissionAuthorityProfile, PermissionResourceAccess, PermissionRisk};
use crate::tools::image_bytes::{
    RemoteImageResource, prepare, read_remote, remote_permission_resource, resolve_remote,
    unresolved_remote_permission_resource,
};
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent, PermissionScopes,
    PlanModeAccess, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{
    BoxFuture, DescriptionContext, ToolAudience, ToolContext, relative_path, resolve_path,
};
use crate::types::{ToolInput, ToolOutput};
use caudra_providers::ImageSource;
use caudra_providers::openai_images::{ImageQuality, ImageRequest, generate};
use caudra_storage::StateDir;

pub const DESCRIPTION: &str = "Generate a raster image from a text prompt and save it as a PNG. Use for AI-created bitmap visuals: illustrations, textures, sprites, photos, and mockups. Requires a ChatGPT subscription login (`caudra auth login openai`) and bills against that plan, not API credits.

- Do not use when the asset is better authored directly as SVG, HTML/CSS, or canvas, or when extending an existing icon or logo system.
- Returns the saved path only. Call `view_image` on it when you need to see the result.
- One image per call. For several distinct assets, call once per asset.
- Reference images are passed with `images` and are described in prompt order; label their roles in `prompt`, e.g. \"Image 1: style reference\".
- Never overwrites: local outputs are versioned; remote output conflicts require a newly authorized path.";

const MAX_VERSION: u32 = 999;
const REMOTE_OUTPUT_CONFLICT: &str =
    "remote output already exists; choose and authorize a different path";
const CONFLICT_CODE: &str = "conflict";
const PNG_EXTENSION: &str = "png";
const SAVED: &str = "Generated image saved to";
const VIEW_HINT: &str = "Call view_image on it to see the result.";
/// A prompt is prose, and the model writes it with the emphasis and the
/// backticks prose is written with.
const PROMPT_LANGUAGE: &str = "markdown";

static PROMPT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Description of the image to generate.",
};
static OUT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Output file path, relative to the project directory unless absolute. Written as a PNG.",
};
static QUALITY_PARAM: ParamSchema = ParamSchema::Enum {
    variants: &["low", "medium", "high", "xhigh", "max", "auto"],
    description: "Generation quality. Defaults to auto.",
};
static SIZE_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Image size, either `auto` or `WIDTHxHEIGHT`. Width and height must be multiples of 16, the long edge at most 3840, the long-to-short ratio at most 3:1, and the total between 655,360 and 8,294,400 pixels.",
};
static IMAGE_PATH_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "",
};
static IMAGES_PARAM: ParamSchema = ParamSchema::Array {
    items: &IMAGE_PATH_PARAM,
    description: "Reference image paths, relative to the project directory unless absolute.",
};
static PROPERTIES: &[Property] = &[
    ("prompt", &PROMPT_PARAM, true, &[]),
    ("out", &OUT_PARAM, true, &["path", "file_path"]),
    ("quality", &QUALITY_PARAM, false, &[]),
    ("size", &SIZE_PARAM, false, &[]),
    ("images", &IMAGES_PARAM, false, &["reference_images"]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct ImageGenerate;

impl Tool for ImageGenerate {
    fn name(&self) -> &str {
        crate::tools::IMAGE_GENERATE_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN | ToolAudience::GENERAL_SUB
    }

    fn tool_kind(&self) -> Option<&str> {
        Some("write")
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        parse_call(input).map(|call| Box::new(call) as Box<dyn ToolInvocation>)
    }
}

fn parse_call(input: &Value) -> Result<ImageGenerateCall, ParseError> {
    let input = validate(&SCHEMA, input.clone())?;
    let prompt = required_str(&input, "prompt")?.to_owned();
    let raw_out = required_str(&input, "out")?.to_owned();
    let out = resolve_path(&raw_out).map_err(ParseError::custom)?;

    let quality = match input.get("quality").and_then(Value::as_str) {
        Some(raw) => ImageQuality::from_wire(raw)
            .ok_or_else(|| ParseError::custom(format!("unknown quality '{raw}'")))?,
        None => ImageQuality::default(),
    };

    let raw_references = match input.get("images").and_then(Value::as_array) {
        Some(paths) => paths
            .iter()
            .map(|path| {
                path.as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| ParseError::custom("images entries must be strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let references = raw_references
        .iter()
        .map(|path| resolve_path(path).map_err(ParseError::custom))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ImageGenerateCall {
        prompt,
        raw_out,
        out,
        quality,
        size: input
            .get("size")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        references,
        raw_references,
        remote: Mutex::new(None),
    })
}

fn required_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ParseError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ParseError::custom(format!("{key} is required")))
}

struct ImageGenerateCall {
    prompt: String,
    raw_out: String,
    out: String,
    quality: ImageQuality,
    size: Option<String>,
    references: Vec<String>,
    raw_references: Vec<String>,
    remote: Mutex<Option<RemoteGeneratePlan>>,
}

#[derive(Clone)]
struct RemoteGeneratePlan {
    out: WorkspacePath,
    references: Vec<RemoteImageResource>,
}

impl ToolInvocation for ImageGenerateCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(relative_path(&self.out)))
    }

    /// The same value the stream drew, so the prompt the reader was watching
    /// does not move when the call starts. It stays for the whole generation,
    /// which is the long part, and the header's parentheses stay free of a
    /// payload no row could hold.
    fn start_input(&self) -> Option<ToolInput> {
        Some(ToolInput::Code {
            language: PROMPT_LANGUAGE.to_owned(),
            code: self.prompt.clone(),
        })
    }

    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.out))
    }

    fn mutation_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        if ctx.workspace_session.is_some() {
            Vec::new()
        } else {
            vec![PathBuf::from(&self.out)]
        }
    }

    fn read_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        if ctx.workspace_session.is_some() {
            Vec::new()
        } else {
            self.references.iter().map(PathBuf::from).collect()
        }
    }

    fn plan_mode_access(&self) -> PlanModeAccess {
        PlanModeAccess::Refused
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(std::future::ready(Some(PermissionScopes::single(
            self.out.clone(),
        ))))
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, String>> {
        Box::pin(async move {
            let Some(session) = ctx.workspace_session.as_ref() else {
                return Ok(None);
            };
            let out = WorkspacePath::new(self.raw_out.clone())
                .map_err(|error| format!("invalid remote output path: {error}"))?;
            let mut references = Vec::with_capacity(self.raw_references.len());
            for raw in &self.raw_references {
                let path = WorkspacePath::new(raw.clone())
                    .map_err(|error| format!("invalid remote reference path: {error}"))?;
                references.push(resolve_remote(session, &path).await?);
            }
            let plan = RemoteGeneratePlan { out, references };
            let intent = remote_generate_intent(session, &plan);
            *self.remote.lock().await = Some(plan);
            Ok(Some(intent))
        })
    }

    fn remote_workspace_effect(&self, ctx: &ToolContext) -> bool {
        ctx.workspace_session.is_some()
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            match self.run(ctx).await {
                Ok((output, Some(path))) => ToolExecResult::from(Ok(output))
                    .with_written_path(Some(path))
                    .with_remote_written_paths(),
                Ok((output, None)) => ToolExecResult::from(Ok(output)),
                Err(message) => ToolExecResult::from(Err(message)),
            }
        })
    }
}

impl ImageGenerateCall {
    async fn run(self, ctx: &ToolContext) -> Result<(ToolOutput, Option<String>), String> {
        let remote_plan = if let Some(session) = ctx.workspace_session.as_ref() {
            Some(match self.remote.lock().await.clone() {
                Some(plan) => plan,
                None => self.resolve_remote_plan(session).await?,
            })
        } else {
            None
        };
        let references = match (ctx.workspace_session.as_ref(), remote_plan.as_ref()) {
            (Some(session), Some(plan)) => {
                read_remote_references(session, &plan.references).await?
            }
            _ => {
                let paths = self.references.clone();
                smol::unblock(move || read_references(&paths)).await?
            }
        };
        let state_dir = StateDir::resolve().map_err(|e| e.to_string())?;

        let bytes = generate(
            &state_dir,
            &ImageRequest {
                prompt: &self.prompt,
                model: ctx.config.image_model,
                quality: self.quality,
                size: self.size.as_deref(),
                references: &references,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

        match (ctx.workspace_session.as_ref(), remote_plan) {
            (Some(session), Some(plan)) => {
                let saved = save_remote(session, &plan.out, bytes).await?;
                let path = saved.path.as_str().to_owned();
                Ok((remote_saved_output(&saved, &plan.out), Some(path)))
            }
            _ => {
                let requested = self.out.clone();
                smol::unblock(move || save(&requested, &bytes))
                    .await
                    .map(|output| (output, None))
            }
        }
    }

    async fn resolve_remote_plan(
        &self,
        session: &WorkspaceSession,
    ) -> Result<RemoteGeneratePlan, String> {
        let out = WorkspacePath::new(self.raw_out.clone())
            .map_err(|error| format!("invalid remote output path: {error}"))?;
        let mut references = Vec::with_capacity(self.raw_references.len());
        for raw in &self.raw_references {
            let path = WorkspacePath::new(raw.clone())
                .map_err(|error| format!("invalid remote reference path: {error}"))?;
            references.push(resolve_remote(session, &path).await?);
        }
        Ok(RemoteGeneratePlan { out, references })
    }
}

fn read_references(paths: &[String]) -> Result<Vec<ImageSource>, String> {
    paths
        .iter()
        .map(|path| prepare(path).map(|image| image.source))
        .collect()
}

async fn read_remote_references(
    session: &WorkspaceSession,
    resources: &[RemoteImageResource],
) -> Result<Vec<ImageSource>, String> {
    let mut references = Vec::with_capacity(resources.len());
    for resource in resources {
        references.push(read_remote(session, resource).await?.source);
    }
    Ok(references)
}

fn remote_generate_intent(
    session: &WorkspaceSession,
    plan: &RemoteGeneratePlan,
) -> PermissionIntent {
    let mut resources = Vec::with_capacity(plan.references.len() + 1);
    resources.push(unresolved_remote_permission_resource(
        session,
        &plan.out,
        PermissionResourceAccess::Write,
    ));
    resources.extend(plan.references.iter().map(|reference| {
        remote_permission_resource(
            session,
            &reference.scope,
            &reference.path,
            PermissionResourceAccess::Read,
            false,
        )
    }));
    PermissionIntent::new(
        PermissionScopes::single(format!("generate remote image {}", plan.out)),
        resources,
        PermissionRisk::High,
    )
    .with_authority(PermissionAuthorityProfile::RemoteResource)
}

#[derive(Debug)]
struct RemoteSavedImage {
    path: WorkspacePath,
    resource_id: ResourceId,
    revision: ResourceRevision,
    versioned: bool,
}

async fn save_remote(
    session: &WorkspaceSession,
    requested: &WorkspacePath,
    bytes: Vec<u8>,
) -> Result<RemoteSavedImage, String> {
    let service = session
        .workspace()
        .services()
        .mutation
        .as_ref()
        .ok_or_else(|| "remote image writes are unavailable".to_owned())?;
    {
        let candidate = requested.clone();
        let request = MutationRequest {
            mutations: vec![Mutation::Write {
                path: candidate.clone(),
                content: WriteContent::Bytes(bytes.clone()),
                condition: MutationCondition::MustNotExist,
            }],
        };
        let status = match service
            .execute(session.binding(), session.cursor(), &request)
            .await
        {
            Ok(status) => status,
            Err(caudra_workspace::WorkspaceError::Conflict) => {
                return Err(REMOTE_OUTPUT_CONFLICT.into());
            }
            Err(error) => return Err(format!("cannot write remote image {candidate}: {error}")),
        };
        let result = match status.state {
            OperationState::Completed { result, .. } => result,
            OperationState::Failed {
                error,
                side_effects_possible: false,
            } if error.code.as_str() == CONFLICT_CODE => return Err(REMOTE_OUTPUT_CONFLICT.into()),
            OperationState::Failed { error, .. } => {
                return Err(format!(
                    "remote image write failed (code {})",
                    error.code.as_str()
                ));
            }
            OperationState::Cancelled { .. } => {
                return Err("remote image write was cancelled".to_owned());
            }
            OperationState::Indeterminate { .. } => {
                return Err("remote image write outcome is indeterminate".to_owned());
            }
            OperationState::NeverSeen
            | OperationState::Prepared
            | OperationState::Running
            | OperationState::Forgotten => {
                return Err("remote image write did not reach a durable terminal state".to_owned());
            }
        };
        if !result.committed
            || result.rolled_back
            || result.results.len() != 1
            || result.results[0].kind != MutationKind::Create
            || result.results[0].path != candidate
            || result.results[0].destination.is_some()
        {
            return Err("remote image write returned an invalid mutation result".to_owned());
        }
        let mutation_revision = result.results[0]
            .revision
            .as_ref()
            .ok_or_else(|| "remote image write returned no revision".to_owned())?;
        let resource = resolve_remote(session, &candidate).await?;
        if &resource.revision != mutation_revision || resource.size_bytes != bytes.len() as u64 {
            return Err("remote image write could not be verified".to_owned());
        }
        Ok(RemoteSavedImage {
            path: resource.path,
            resource_id: resource.resource_id,
            revision: resource.revision,
            versioned: false,
        })
    }
}

fn remote_saved_output(saved: &RemoteSavedImage, requested: &WorkspacePath) -> ToolOutput {
    let mut message = format!(
        "{SAVED} {} (resource {}, revision {}). {VIEW_HINT}",
        saved.path,
        saved.resource_id.as_str(),
        saved.revision.as_str()
    );
    if saved.versioned {
        message.push_str(&format!(
            " The requested path {requested} already existed, so the new image was versioned rather than overwriting it."
        ));
    }
    ToolOutput::Plain(message.into())
}

fn save(requested: &str, bytes: &[u8]) -> Result<ToolOutput, String> {
    if let Some(parent) = Path::new(requested).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let saved = non_overwriting_path(requested)?;
    std::fs::write(&saved, bytes).map_err(|e| format!("cannot write {saved}: {e}"))?;

    let mut message = format!("{SAVED} {}. {VIEW_HINT}", relative_path(&saved));
    if saved != requested {
        message.push_str(&format!(
            " The requested path {} already existed, so the new image was versioned rather than overwriting it.",
            relative_path(requested)
        ));
    }
    Ok(ToolOutput::Plain(message.into()))
}

/// Generations are expensive and unreproducible, so an existing file is never
/// clobbered: `art.png` becomes `art-v2.png`, then `art-v3.png`.
fn non_overwriting_path(requested: &str) -> Result<String, String> {
    if !Path::new(requested).exists() {
        return Ok(requested.to_owned());
    }
    let path = Path::new(requested);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| PNG_EXTENSION.to_owned());

    for version in 2..=MAX_VERSION {
        let candidate = dir.join(format!("{stem}-v{version}.{ext}"));
        if !candidate.exists() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Err(format!(
        "every filename from {stem}-v2.{ext} to {stem}-v{MAX_VERSION}.{ext} is taken in {}",
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ByteContent, CancellationResult, CwdHandle,
        ListPage, ListRequest, MutationEntryResult, MutationResult, OperationError,
        OperationHandle, OperationPhase, OperationProgress, OperationStatus, ProjectIdentity,
        ProjectKey, ReadBytesRequest, ReadTextRequest, ResourceScope, ResourceSelector,
        SequenceMetadata, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        TextContent, WorkspaceCapabilities, WorkspaceCapability, WorkspaceCursor, WorkspaceError,
        WorkspaceHandle, WorkspaceMutationService, WorkspaceReadService, WorkspaceResource,
        WorkspaceServices,
    };
    use test_case::test_case;

    use super::*;
    use crate::tools::image_bytes::test_support::{png, write};
    use caudra_providers::ImageMediaType;

    const PROMPT: &str = "a red square";
    /// Schema-layer wording: `validate` rejects these before `parse_call` runs.
    const MSG_MISSING: &str = "required, expected";
    const MSG_EXPECTED_ONE_OF: &str = "expected one of";
    const BLANK_PROMPT: &str = "prompt is required";
    const EXHAUSTED: &str = "is taken in";
    const REMOTE_OUT: &str = "generated/remote-only-output-canary.png";
    const REMOTE_SECRET: &str = "https://secret.example bearer-secret";

    #[derive(Clone, Copy)]
    enum MutationOutcome {
        Complete,
        Conflict,
        Indeterminate,
        FailedWithSecret,
        ReplaceResult,
    }

    struct ImageMutationService {
        binding: SessionWorkspaceBinding,
        bytes: Vec<u8>,
        conflict_first: bool,
        outcome: MutationOutcome,
        attempts: StdMutex<Vec<Mutation>>,
        latest_path: StdMutex<Option<WorkspacePath>>,
    }

    impl ImageMutationService {
        fn resource(&self, path: WorkspacePath) -> WorkspaceResource {
            WorkspaceResource {
                project: self.binding.project().clone(),
                scope: ResourceScope::new(
                    vec![ResourceId::new("root").unwrap()],
                    ResourceId::new("generated-resource").unwrap(),
                )
                .unwrap(),
                path: Some(path),
                kind: caudra_workspace::ResourceKind::File,
                revision: Some(ResourceRevision::new("generated-revision").unwrap()),
                size_bytes: Some(self.bytes.len() as u64),
            }
        }

        fn status(&self, state: OperationState<MutationResult>) -> OperationStatus<MutationResult> {
            OperationStatus {
                handle: OperationHandle {
                    preparation_id: caudra_workspace::OperationId::new("prepare").unwrap(),
                    invocation_id: Some(caudra_workspace::OperationId::new("invoke").unwrap()),
                    execution_id: Some(caudra_workspace::OperationId::new("execute").unwrap()),
                    expires_at_unix_ms: None,
                },
                state,
                progress: Vec::<OperationProgress>::new(),
                progress_metadata: SequenceMetadata {
                    first_retained_sequence: None,
                    next_sequence: 0,
                    gap_before_first: false,
                },
            }
        }
    }

    #[async_trait]
    impl WorkspaceReadService for ImageMutationService {
        async fn resolve(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            Ok(self.resource(path.clone()))
        }

        async fn resolve_directory(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _path: &WorkspacePath,
        ) -> Result<caudra_workspace::ResolvedWorkspaceDirectory, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }

        async fn stat(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            resource: &ResourceSelector,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            assert!(
                matches!(resource, ResourceSelector::Id(id) if id.as_str() == "generated-resource")
            );
            let path = self.latest_path.lock().unwrap().clone().unwrap();
            Ok(self.resource(path))
        }

        async fn list(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ListRequest,
        ) -> Result<ListPage, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }

        async fn read_text(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ReadTextRequest,
        ) -> Result<TextContent, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }

        async fn read_bytes(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ReadBytesRequest,
        ) -> Result<ByteContent, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
    }

    #[async_trait]
    impl WorkspaceMutationService for ImageMutationService {
        async fn execute(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &MutationRequest,
        ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
            let mutation = request.mutations.first().unwrap().clone();
            let Mutation::Write {
                path,
                content,
                condition,
            } = &mutation
            else {
                panic!("image generation must only write")
            };
            assert_eq!(condition, &MutationCondition::MustNotExist);
            assert!(matches!(content, WriteContent::Bytes(bytes) if bytes == &self.bytes));
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push(mutation.clone());
            if self.conflict_first && attempts.len() == 1 {
                return Err(WorkspaceError::Conflict);
            }
            *self.latest_path.lock().unwrap() = Some(path.clone());
            let state = match self.outcome {
                MutationOutcome::Conflict => OperationState::Failed {
                    error: OperationError {
                        code: caudra_workspace::OperationId::new(CONFLICT_CODE).unwrap(),
                        message: "condition did not match".to_owned(),
                    },
                    side_effects_possible: false,
                },
                MutationOutcome::Indeterminate => OperationState::Indeterminate {
                    side_effects_possible: true,
                },
                MutationOutcome::FailedWithSecret => OperationState::Failed {
                    error: OperationError {
                        code: caudra_workspace::OperationId::new("remote-failure").unwrap(),
                        message: REMOTE_SECRET.to_owned(),
                    },
                    side_effects_possible: true,
                },
                MutationOutcome::Complete | MutationOutcome::ReplaceResult => {
                    OperationState::Completed {
                        result: MutationResult {
                            committed: true,
                            rolled_back: false,
                            atomic_across_files: true,
                            results: vec![MutationEntryResult {
                                kind: if matches!(self.outcome, MutationOutcome::ReplaceResult) {
                                    MutationKind::Write
                                } else {
                                    MutationKind::Create
                                },
                                path: path.clone(),
                                destination: None,
                                revision: Some(
                                    ResourceRevision::new("generated-revision").unwrap(),
                                ),
                            }],
                        },
                        side_effects_possible: true,
                    }
                }
            };
            Ok(self.status(state))
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }

        async fn cancel(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            Ok(CancellationResult {
                state: OperationPhase::Cancelled,
                cancellation_requested: true,
            })
        }
    }

    fn remote_session(
        bytes: Vec<u8>,
        conflict_first: bool,
        outcome: MutationOutcome,
    ) -> (WorkspaceSession, Arc<ImageMutationService>) {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-anchor").unwrap(),
            "test-authority",
            "test-workspace",
            "test-generation",
            "test-namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap();
        let project = ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap());
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("session").unwrap(),
            authority.clone(),
            principal,
            project,
        )
        .unwrap();
        let service = Arc::new(ImageMutationService {
            binding: binding.clone(),
            bytes,
            conflict_first,
            outcome,
            attempts: StdMutex::new(Vec::new()),
            latest_path: StdMutex::new(None),
        });
        let workspace = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::from([
                WorkspaceCapability::Resolve,
                WorkspaceCapability::Stat,
                WorkspaceCapability::MutationExecute,
            ]),
            WorkspaceServices {
                read: Some(service.clone()),
                mutation: Some(service.clone()),
                ..WorkspaceServices::default()
            },
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").unwrap()),
            1,
            CwdHandle::new("cwd").unwrap(),
        );
        (
            WorkspaceSession::new(workspace, binding, cursor).unwrap(),
            service,
        )
    }

    fn call(input: Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        ImageGenerate.parse(&input)
    }

    fn text(output: &ToolOutput) -> String {
        match output {
            ToolOutput::Plain(t) => t.text.clone(),
            other => panic!("expected plain output, got {other:?}"),
        }
    }

    #[test]
    fn a_fresh_path_is_used_as_requested() {
        let dir = tempfile::tempdir().unwrap();
        let requested = dir.path().join("art.png").to_string_lossy().into_owned();

        assert_eq!(non_overwriting_path(&requested).unwrap(), requested);
    }

    #[test]
    fn an_existing_path_is_versioned_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let requested = write(&dir, "art.png", &png(1, 1));

        let picked = non_overwriting_path(&requested).unwrap();

        assert_eq!(picked, dir.path().join("art-v2.png").to_string_lossy());
        assert_eq!(std::fs::read(&requested).unwrap(), png(1, 1));
    }

    #[test]
    fn versioning_skips_over_versions_that_already_exist() {
        let dir = tempfile::tempdir().unwrap();
        let requested = write(&dir, "art.png", &png(1, 1));
        write(&dir, "art-v2.png", &png(1, 1));
        write(&dir, "art-v3.png", &png(1, 1));

        let picked = non_overwriting_path(&requested).unwrap();

        assert_eq!(picked, dir.path().join("art-v4.png").to_string_lossy());
    }

    #[test]
    fn an_exhausted_version_range_is_an_error_not_an_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let requested = write(&dir, "art.png", &[]);
        for version in 2..=MAX_VERSION {
            write(&dir, &format!("art-v{version}.png"), &[]);
        }

        let error = non_overwriting_path(&requested).unwrap_err();

        assert!(error.contains(EXHAUSTED), "{error}");
    }

    #[test]
    fn saving_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/art.png");
        let requested = nested.to_string_lossy().into_owned();

        let output = save(&requested, &png(2, 2)).unwrap();

        assert!(nested.exists());
        assert!(text(&output).contains(SAVED), "{output:?}");
    }

    #[test]
    fn the_saved_message_points_the_model_at_view_image() {
        let dir = tempfile::tempdir().unwrap();
        let requested = dir.path().join("art.png").to_string_lossy().into_owned();

        let message = text(&save(&requested, &png(2, 2)).unwrap());

        assert!(message.contains(VIEW_HINT), "{message}");
        assert!(!message.contains("versioned"), "{message}");
    }

    #[test]
    fn the_saved_message_explains_a_version_bump() {
        let dir = tempfile::tempdir().unwrap();
        let requested = write(&dir, "art.png", &png(1, 1));

        let message = text(&save(&requested, &png(2, 2)).unwrap());

        assert!(message.contains("already existed"), "{message}");
        assert!(message.contains("art-v2.png"), "{message}");
    }

    #[test]
    fn schema_requires_a_prompt_and_an_output_path() {
        let schema = to_json_schema(&SCHEMA);
        assert_eq!(schema["required"], serde_json::json!(["prompt", "out"]));
    }

    #[test_case(serde_json::json!({"out": "a.png"}), MSG_MISSING ; "missing_prompt")]
    #[test_case(serde_json::json!({"prompt": PROMPT}), MSG_MISSING ; "missing_out")]
    #[test_case(serde_json::json!({"prompt": "  ", "out": "a.png"}), BLANK_PROMPT ; "blank_prompt_survives_the_schema")]
    #[test_case(serde_json::json!({"prompt": PROMPT, "out": "a.png", "quality": "ultra"}), MSG_EXPECTED_ONE_OF ; "bad_quality")]
    fn invalid_input_is_rejected_at_parse_time(input: Value, expected: &str) {
        let error = call(input).err().expect("parse should fail").to_string();
        assert!(error.contains(expected), "{error}");
    }

    /// The schema enum is what the model is shown; `ImageQuality` is what the
    /// request carries. If they drift, a quality the model is told to use gets
    /// rejected at runtime instead of at the schema boundary.
    #[test]
    fn the_advertised_qualities_are_exactly_the_ones_the_backend_accepts() {
        let advertised = match QUALITY_PARAM {
            ParamSchema::Enum { variants, .. } => variants,
            _ => panic!("quality must stay an enum so the model sees its options"),
        };
        let supported: Vec<&str> = ImageQuality::ALL.iter().map(|q| q.as_str()).collect();

        assert_eq!(advertised, supported.as_slice());
    }

    #[test]
    fn out_accepts_the_path_alias() {
        let parsed = validate(
            &SCHEMA,
            serde_json::json!({"prompt": PROMPT, "path": "a.png"}),
        )
        .expect("alias should resolve");
        assert_eq!(parsed["out"], serde_json::json!("a.png"));
    }

    #[test]
    fn quality_defaults_to_auto_when_omitted() {
        let input = serde_json::json!({"prompt": PROMPT, "out": "a.png"});
        assert!(call(input).is_ok());
        assert_eq!(ImageQuality::default(), ImageQuality::Auto);
    }

    const PROMPT_IS_THE_BODY: &str = "a generation carries its prompt as the card's body, so the \
        text the reader watched arrive is the text that stays through the wait; the header's \
        parentheses are no place for a payload";

    /// The reported bug: the whole prompt was returned as the start annotation
    /// and `push_header` writes an annotation as ` (…)` with no cap, so a
    /// paragraph wrapped across the header the moment the call started.
    #[test]
    fn a_generation_carries_its_prompt_as_a_body_and_annotates_nothing() {
        let parsed = parse_call(&serde_json::json!({ "prompt": PROMPT, "out": "a.png" }))
            .expect("parse should succeed");

        assert_eq!(
            parsed.start_input(),
            Some(ToolInput::Code {
                language: PROMPT_LANGUAGE.to_owned(),
                code: PROMPT.to_owned(),
            }),
            "{PROMPT_IS_THE_BODY}"
        );
        assert_eq!(parsed.start_annotation(), None, "{PROMPT_IS_THE_BODY}");
    }

    #[test]
    fn reference_paths_are_resolved_to_absolute_paths_at_parse_time() {
        let parsed = parse_call(&serde_json::json!({
            "prompt": PROMPT,
            "out": "a.png",
            "images": ["ref.png"],
        }))
        .expect("parse should succeed");

        assert_eq!(parsed.references.len(), 1);
        let reference = Path::new(&parsed.references[0]);
        assert!(reference.is_absolute(), "{reference:?}");
        assert!(reference.ends_with("ref.png"), "{reference:?}");
        assert!(Path::new(&parsed.out).is_absolute(), "{}", parsed.out);
    }

    #[test]
    fn a_reference_that_is_not_an_image_fails_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "notes.txt", b"just text");

        let error = read_references(&[path]).unwrap_err();

        assert!(error.contains("is not an image"), "{error}");
    }

    #[test]
    fn references_are_prepared_in_the_order_given() {
        let dir = tempfile::tempdir().unwrap();
        let first = write(&dir, "one.png", &png(2, 2));
        let second = write(&dir, "two.png", &png(3, 3));

        let sources = read_references(&[first, second]).unwrap();

        assert_eq!(sources.len(), 2);
        assert!(sources.iter().all(|s| s.media_type == ImageMediaType::Png));
        assert_ne!(sources[0].data, sources[1].data);
    }

    #[test]
    fn generation_is_blocked_in_plan_mode() {
        let parsed = call(serde_json::json!({"prompt": PROMPT, "out": "a.png"})).unwrap();
        assert_eq!(parsed.plan_mode_access(), PlanModeAccess::Refused);
    }

    #[test]
    fn remote_generation_does_not_write_an_unauthorized_version_on_conflict() {
        let bytes = png(2, 2);
        let (session, service) = remote_session(bytes.clone(), true, MutationOutcome::Complete);
        let requested = WorkspacePath::new(REMOTE_OUT).unwrap();

        let error = smol::block_on(save_remote(&session, &requested, bytes)).unwrap_err();
        assert_eq!(error, REMOTE_OUTPUT_CONFLICT);
        let attempts = service.attempts.lock().unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(attempts.iter().all(|mutation| matches!(
            mutation,
            Mutation::Write {
                condition: MutationCondition::MustNotExist,
                ..
            }
        )));
        assert!(!Path::new(REMOTE_OUT).exists());
    }

    #[test]
    fn remote_generation_output_authority_is_path_and_cursor_specific() {
        let (session, _) = remote_session(Vec::new(), false, MutationOutcome::Complete);
        let first_path = WorkspacePath::new("generated/first.png").unwrap();
        let second_path = WorkspacePath::new("generated/second.png").unwrap();
        let first = remote_generate_intent(
            &session,
            &RemoteGeneratePlan {
                out: first_path.clone(),
                references: Vec::new(),
            },
        );
        let second = remote_generate_intent(
            &session,
            &RemoteGeneratePlan {
                out: second_path,
                references: Vec::new(),
            },
        );

        assert_ne!(first.resources[0].value, second.resources[0].value);
        assert_ne!(first.resources[0].value, "root");
        assert!(!first.resources[0].value.contains(first_path.as_str()));
        assert!(matches!(
            &first.resources[0].kind,
            crate::permissions::PermissionResourceKind::RemoteFile { identity }
                if identity.authority.workspace_generation() == "test-generation"
                    && identity.principal.subject() == "principal"
                    && identity.project.key().as_str() == "project"
        ));
    }

    #[test]
    fn remote_generation_does_not_retry_a_terminal_create_conflict() {
        let bytes = png(2, 2);
        let (conflict_session, conflict_service) =
            remote_session(bytes.clone(), false, MutationOutcome::Conflict);
        let requested = WorkspacePath::new(REMOTE_OUT).unwrap();

        let error =
            smol::block_on(save_remote(&conflict_session, &requested, bytes.clone())).unwrap_err();

        assert_eq!(error, REMOTE_OUTPUT_CONFLICT);
        assert_eq!(conflict_service.attempts.lock().unwrap().len(), 1);
    }

    #[test]
    fn remote_generation_rejects_a_replace_result_for_a_create() {
        let bytes = png(2, 2);
        let (session, _) = remote_session(bytes.clone(), false, MutationOutcome::ReplaceResult);
        let error = smol::block_on(save_remote(
            &session,
            &WorkspacePath::new(REMOTE_OUT).unwrap(),
            bytes,
        ))
        .unwrap_err();

        assert!(error.contains("invalid mutation result"), "{error}");
    }

    #[test]
    fn remote_generation_never_retries_an_indeterminate_mutation() {
        let bytes = png(2, 2);
        let (session, service) =
            remote_session(bytes.clone(), false, MutationOutcome::Indeterminate);
        let error = smol::block_on(save_remote(
            &session,
            &WorkspacePath::new(REMOTE_OUT).unwrap(),
            bytes,
        ))
        .unwrap_err();

        assert!(error.contains("outcome is indeterminate"), "{error}");
        assert_eq!(service.attempts.lock().unwrap().len(), 1);
    }

    #[test]
    fn remote_generation_does_not_leak_transport_secrets() {
        let bytes = png(2, 2);
        let (session, _) = remote_session(bytes.clone(), false, MutationOutcome::FailedWithSecret);
        let error = smol::block_on(save_remote(
            &session,
            &WorkspacePath::new(REMOTE_OUT).unwrap(),
            bytes,
        ))
        .unwrap_err();

        assert!(!error.contains(REMOTE_SECRET), "{error}");
        assert!(!error.contains("secret.example"), "{error}");
        assert!(error.contains("remote-failure"), "{error}");
    }

    #[test]
    fn remote_result_contains_only_display_path_and_opaque_file_identity() {
        let saved = RemoteSavedImage {
            path: WorkspacePath::new(REMOTE_OUT).unwrap(),
            resource_id: ResourceId::new("opaque-resource").unwrap(),
            revision: ResourceRevision::new("opaque-revision").unwrap(),
            versioned: false,
        };
        let message = text(&remote_saved_output(
            &saved,
            &WorkspacePath::new(REMOTE_OUT).unwrap(),
        ));

        assert!(message.contains(REMOTE_OUT), "{message}");
        assert!(message.contains("opaque-resource"), "{message}");
        assert!(message.contains("opaque-revision"), "{message}");
        assert!(!message.contains("/tmp/"), "{message}");
        assert!(!message.contains("https://"), "{message}");
        assert!(!message.contains("bearer"), "{message}");
    }
}
