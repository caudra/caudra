//! `view_image`: hand an image file to a vision model.
//!
//! `file_read` can only produce text, so images need their own path. The decode
//! and shrink pipeline lives in `image_bytes`; this module only turns the
//! result into a captioned tool output.

use std::borrow::Cow;
use std::path::Path;

use async_lock::Mutex;
use caudra_workspace::{WorkspacePath, WorkspaceSession};
use serde_json::Value;

use crate::permissions::{PermissionAuthorityProfile, PermissionResourceAccess, PermissionRisk};
use crate::tools::image_bytes::{
    RemoteImageResource, format_size, prepare, read_remote, remote_permission_resource,
    resolve_remote,
};
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent, PermissionScopes, Tool,
    ToolError, ToolExecResult, ToolFailure, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{
    BoxFuture, DescriptionContext, ToolAudience, ToolContext, relative_path, resolve_path,
};
use crate::types::ToolOutput;

pub const DESCRIPTION: &str = "View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `file_read` for images.

- Paths: absolute, relative, or ~/.
- Oversized images are downscaled automatically (animated gif/webp keep only the first frame).";

static PATH_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Path to the image file",
};
static PROPERTIES: &[Property] = &[("path", &PATH_PARAM, true, &["file_path"])];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct ViewImage;

impl Tool for ViewImage {
    fn name(&self) -> &str {
        crate::tools::VIEW_IMAGE_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        // No interpreter audience: the python_execution bridge flattens tool
        // output to text, so the pixels could never reach the model from there.
        ToolAudience::MAIN | ToolAudience::RESEARCH_SUB | ToolAudience::GENERAL_SUB
    }

    fn tool_kind(&self) -> Option<&str> {
        Some("read")
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let raw = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::custom("path is required"))?;
        let local_path = resolve_path(raw).map_err(ParseError::custom)?;
        Ok(Box::new(ViewImageCall {
            raw_path: raw.to_owned(),
            local_path,
            remote: Mutex::new(None),
        }))
    }
}

struct ViewImageCall {
    raw_path: String,
    local_path: String,
    remote: Mutex<Option<RemoteImageResource>>,
}

impl ToolInvocation for ViewImageCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(relative_path(&self.local_path)))
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(std::future::ready(Some(PermissionScopes::single(
            self.local_path.clone(),
        ))))
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async move {
            let Some(session) = ctx.workspace_session.as_ref() else {
                return Ok(None);
            };
            let resource = resolve_remote(session, &remote_path(&self.raw_path)?).await?;
            let intent = remote_read_intent(session, &resource);
            *self.remote.lock().await = Some(resource);
            Ok(Some(intent))
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let result = match ctx.workspace_session.as_ref() {
                Some(session) => self.load_remote(session).await,
                None => {
                    let path = self.local_path;
                    smol::unblock(move || load(&path)).await
                }
            };
            match result {
                Ok(output) => ToolExecResult::from(Ok(output)),
                Err(error) => ToolExecResult::failed(error.failure, error.message),
            }
        })
    }
}

impl ViewImageCall {
    async fn load_remote(&self, session: &WorkspaceSession) -> Result<ToolOutput, ToolError> {
        let resource = match self.remote.lock().await.clone() {
            Some(resource) => resource,
            None => resolve_remote(session, &remote_path(&self.raw_path)?).await?,
        };
        let image = read_remote(session, &resource).await?;
        Ok(ToolOutput::Image {
            text: caption(
                resource.path.as_str(),
                image.bytes,
                image.width,
                image.height,
                &image.note,
            ),
            source: image.source,
        })
    }
}

fn remote_read_intent(
    session: &WorkspaceSession,
    resource: &RemoteImageResource,
) -> PermissionIntent {
    PermissionIntent::new(
        PermissionScopes::single(format!("read remote image {}", resource.path)),
        vec![remote_permission_resource(
            session,
            &resource.scope,
            &resource.path,
            PermissionResourceAccess::Read,
            false,
        )],
        PermissionRisk::Low,
    )
    .with_authority(PermissionAuthorityProfile::RemoteResource)
}

fn remote_path(raw: &str) -> Result<WorkspacePath, ToolError> {
    WorkspacePath::new(raw.to_owned()).map_err(|error| {
        ToolError::new(
            ToolFailure::InvalidInput,
            format!("invalid remote image path: {error}"),
        )
    })
}

fn load(path: &str) -> Result<ToolOutput, ToolError> {
    let image = prepare(path)?;
    Ok(ToolOutput::Image {
        text: caption(path, image.bytes, image.width, image.height, &image.note),
        source: image.source,
    })
}

/// Shortened path, not basename: two `screenshot.png` in different directories
/// must stay distinguishable when several images land in one turn.
fn caption(path: &str, bytes: u64, width: u32, height: u32, note: &str) -> String {
    format!(
        "[image: {} {} {width}x{height}{note}]",
        shorten(path),
        format_size(bytes)
    )
}

fn shorten(path: &str) -> String {
    if Path::new(path).is_absolute() {
        relative_path(path)
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ByteContent, ByteRange, CwdHandle, ListPage,
        ListRequest, ProjectIdentity, ProjectKey, ReadBytesRequest, ReadTextRequest, ResourceId,
        ResourceKind, ResourceRevision, ResourceScope, ResourceSelector, SessionBindingId,
        SessionWorkspaceBinding, SourceTrustAnchor, TextContent, WorkspaceCapabilities,
        WorkspaceCapability, WorkspaceCursor, WorkspaceError, WorkspaceHandle, WorkspacePath,
        WorkspaceReadService, WorkspaceResource, WorkspaceServices,
    };

    use super::*;
    use crate::tools::image_bytes::test_support::{png, write};
    use crate::tools::image_bytes::{MAX_EDGE, MAX_INPUT_BYTES};
    use caudra_providers::ImageMediaType;

    const REMOTE_PATH: &str = "remote-image-local-canary-test.png";
    const RESOURCE_ID: &str = "remote-image-resource";
    const REVISION: &str = "remote-image-revision";

    #[derive(Clone, Copy)]
    enum ReadFault {
        None,
        Ranged,
        Stale,
        Oversized,
    }

    struct ImageReadService {
        bytes: Vec<u8>,
        fault: ReadFault,
        reads: AtomicUsize,
        binding: SessionWorkspaceBinding,
    }

    impl ImageReadService {
        fn resource(&self) -> WorkspaceResource {
            WorkspaceResource {
                project: self.binding.project().clone(),
                scope: ResourceScope::new(
                    vec![ResourceId::new("root").unwrap()],
                    ResourceId::new(RESOURCE_ID).unwrap(),
                )
                .unwrap(),
                path: Some(WorkspacePath::new(REMOTE_PATH).unwrap()),
                kind: ResourceKind::File,
                revision: Some(ResourceRevision::new(REVISION).unwrap()),
                size_bytes: Some(if matches!(self.fault, ReadFault::Oversized) {
                    MAX_INPUT_BYTES + 1
                } else {
                    self.bytes.len() as u64
                }),
            }
        }
    }

    #[async_trait]
    impl WorkspaceReadService for ImageReadService {
        async fn resolve(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            assert_eq!(path.as_str(), REMOTE_PATH);
            Ok(self.resource())
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
            selector: &ResourceSelector,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            assert!(matches!(selector, ResourceSelector::Id(id) if id.as_str() == RESOURCE_ID));
            Ok(self.resource())
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
            request: &ReadBytesRequest,
        ) -> Result<ByteContent, WorkspaceError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.byte_offset, 0);
            assert_eq!(request.if_revision.as_ref().unwrap().as_str(), REVISION);
            let (resource_id, revision, start) = match self.fault {
                ReadFault::Stale => (
                    ResourceId::new("different-resource").unwrap(),
                    ResourceRevision::new("different-revision").unwrap(),
                    0,
                ),
                ReadFault::Ranged => (
                    ResourceId::new(RESOURCE_ID).unwrap(),
                    ResourceRevision::new(REVISION).unwrap(),
                    1,
                ),
                _ => (
                    ResourceId::new(RESOURCE_ID).unwrap(),
                    ResourceRevision::new(REVISION).unwrap(),
                    0,
                ),
            };
            Ok(ByteContent {
                bytes: self.bytes.clone(),
                resource_id,
                revision,
                range: ByteRange {
                    start,
                    end_exclusive: start + self.bytes.len() as u64,
                },
                total_bytes: Some(self.bytes.len() as u64),
                truncated: false,
                next_byte_offset: None,
            })
        }
    }

    fn remote_session(
        bytes: Vec<u8>,
        fault: ReadFault,
    ) -> (WorkspaceSession, Arc<ImageReadService>) {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-anchor").unwrap(),
            "test-authority",
            "test-workspace",
            "test-generation",
            "test-namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), "test-principal").unwrap();
        let project = ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap());
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("session").unwrap(),
            authority.clone(),
            principal,
            project,
        )
        .unwrap();
        let service = Arc::new(ImageReadService {
            bytes,
            fault,
            reads: AtomicUsize::new(0),
            binding: binding.clone(),
        });
        let workspace = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::from([
                WorkspaceCapability::Resolve,
                WorkspaceCapability::Stat,
                WorkspaceCapability::ReadBytes,
            ]),
            WorkspaceServices {
                read: Some(service.clone()),
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

    fn image_text(output: &ToolOutput) -> &str {
        match output {
            ToolOutput::Image { text, .. } => text,
            other => panic!("expected an image output, got {other:?}"),
        }
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320u32 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    #[test]
    fn caption_reports_dimensions_and_media_type_for_a_passthrough_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "small.png", &png(4, 3));
        let output = load(&path).unwrap();

        assert!(image_text(&output).contains("4x3"), "{output:?}");
        assert!(!image_text(&output).contains("downscaled"), "{output:?}");
        match &output {
            ToolOutput::Image { source, .. } => {
                assert_eq!(source.media_type, ImageMediaType::Png)
            }
            other => panic!("expected an image output, got {other:?}"),
        }
    }

    #[test]
    fn caption_records_that_an_oversized_image_was_downscaled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "big.png", &png(MAX_EDGE + 400, 10));
        let text = image_text(&load(&path).unwrap()).to_owned();

        assert!(text.contains(&format!("{MAX_EDGE}x")), "{text}");
        assert!(text.contains("downscaled from"), "{text}");
    }

    #[test]
    fn failures_from_the_shared_pipeline_reach_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "notes.txt", b"just text");
        assert!(load(&path).unwrap_err().message.contains("is not an image"));
    }

    #[test]
    fn schema_requires_path_and_accepts_the_file_path_alias() {
        let schema = to_json_schema(&SCHEMA);
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        let parsed = validate(&SCHEMA, serde_json::json!({"file_path": "/tmp/x.png"})).unwrap();
        assert_eq!(parsed["path"], serde_json::json!("/tmp/x.png"));
    }

    #[test]
    fn remote_view_never_reads_a_same_named_local_canary() {
        let local = png(9, 7);
        let remote = png(2, 3);
        std::fs::write(REMOTE_PATH, local).unwrap();
        let (session, service) = remote_session(remote, ReadFault::None);
        let call = ViewImageCall {
            raw_path: REMOTE_PATH.to_owned(),
            local_path: REMOTE_PATH.to_owned(),
            remote: Mutex::new(None),
        };

        let output = smol::block_on(call.load_remote(&session)).unwrap();
        std::fs::remove_file(REMOTE_PATH).unwrap();

        assert!(image_text(&output).contains("2x3"), "{output:?}");
        assert_eq!(service.reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn remote_view_rejects_stale_and_ranged_transfers() {
        for fault in [ReadFault::Stale, ReadFault::Ranged] {
            let (session, _) = remote_session(png(2, 3), fault);
            let path = WorkspacePath::new(REMOTE_PATH).unwrap();
            let error = smol::block_on(async {
                let resource = resolve_remote(&session, &path).await.unwrap();
                read_remote(&session, &resource).await.unwrap_err()
            });
            assert!(
                error.message.contains("stale or incomplete byte range"),
                "{error}"
            );
        }
    }

    #[test]
    fn remote_view_rejects_an_oversized_resource_before_transfer() {
        let (session, service) = remote_session(Vec::new(), ReadFault::Oversized);
        let error = smol::block_on(resolve_remote(
            &session,
            &WorkspacePath::new(REMOTE_PATH).unwrap(),
        ))
        .unwrap_err();

        assert!(error.message.contains("too large to view"), "{error}");
        assert_eq!(service.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn remote_view_applies_the_decode_bomb_guard() {
        let mut bomb = png(1, 1);
        bomb[16..20].copy_from_slice(&10_000u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&10_000u32.to_be_bytes());
        let header_crc = crc32(&bomb[12..29]);
        bomb[29..33].copy_from_slice(&header_crc.to_be_bytes());
        let (session, _) = remote_session(bomb, ReadFault::None);
        let error = smol::block_on(async {
            let resource = resolve_remote(&session, &WorkspacePath::new(REMOTE_PATH).unwrap())
                .await
                .unwrap();
            read_remote(&session, &resource).await.unwrap_err()
        });

        assert!(
            error.message.contains("image too large to decode"),
            "{error}"
        );
    }

    #[test]
    fn remote_view_permission_uses_opaque_remote_identity_not_a_local_path() {
        let (session, _) = remote_session(png(2, 3), ReadFault::None);
        let resource = smol::block_on(resolve_remote(
            &session,
            &WorkspacePath::new(REMOTE_PATH).unwrap(),
        ))
        .unwrap();

        let intent = remote_read_intent(&session, &resource);

        assert_eq!(intent.authority, PermissionAuthorityProfile::RemoteResource);
        assert_eq!(intent.resources.len(), 1);
        assert!(matches!(
            &intent.resources[0].kind,
            crate::permissions::PermissionResourceKind::RemoteFile {
                identity,
            } if identity.authority.trust_anchor().as_str() == "test-anchor"
                && identity.authority.server_id() == "test-authority"
                && identity.authority.workspace_id() == "test-workspace"
                && identity.authority.workspace_generation() == "test-generation"
                && identity.authority.resource_namespace_version() == "test-namespace"
                && identity.principal.subject() == "test-principal"
                && identity.project.key().as_str() == "project"
        ));
        assert_eq!(
            intent.resources[0].value,
            format!("root\u{1f}{RESOURCE_ID}")
        );
        assert!(!intent.resources[0].value.contains(REMOTE_PATH));
    }
}
