//! `view_image`: hand an image file to a vision model.
//!
//! `file_read` can only produce text, so images need their own path. The decode
//! and shrink pipeline lives in `image_bytes`; this module only turns the
//! result into a captioned tool output.

use std::borrow::Cow;
use std::path::Path;

use serde_json::Value;

use crate::tools::image_bytes::{format_size, prepare};
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes, Tool, ToolExecResult,
    ToolInvocation,
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
        let path = resolve_path(raw).map_err(ParseError::custom)?;
        Ok(Box::new(ViewImageCall { path }))
    }
}

struct ViewImageCall {
    path: String,
}

impl ToolInvocation for ViewImageCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(relative_path(&self.path)))
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(std::future::ready(Some(PermissionScopes::single(
            self.path.clone(),
        ))))
    }

    fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            match smol::unblock(move || load(&self.path)).await {
                Ok(output) => ToolExecResult::from(Ok(output)),
                Err(message) => ToolExecResult::from(Err(message)),
            }
        })
    }
}

fn load(path: &str) -> Result<ToolOutput, String> {
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
    use super::*;
    use crate::tools::image_bytes::MAX_EDGE;
    use crate::tools::image_bytes::test_support::{png, write};
    use caudra_providers::ImageMediaType;

    fn image_text(output: &ToolOutput) -> &str {
        match output {
            ToolOutput::Image { text, .. } => text,
            other => panic!("expected an image output, got {other:?}"),
        }
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
        assert!(load(&path).unwrap_err().contains("is not an image"));
    }

    #[test]
    fn schema_requires_path_and_accepts_the_file_path_alias() {
        let schema = to_json_schema(&SCHEMA);
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        let parsed = validate(&SCHEMA, serde_json::json!({"file_path": "/tmp/x.png"})).unwrap();
        assert_eq!(parsed["path"], serde_json::json!("/tmp/x.png"));
    }
}
