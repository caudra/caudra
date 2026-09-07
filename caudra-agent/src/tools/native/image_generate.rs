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

use serde_json::Value;

use crate::tools::image_bytes::prepare;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes, PlanModeAccess, Tool,
    ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{
    BoxFuture, DescriptionContext, ToolAudience, ToolContext, relative_path, resolve_path,
};
use crate::types::ToolOutput;
use caudra_providers::ImageSource;
use caudra_providers::openai_images::{ImageQuality, ImageRequest, generate};
use caudra_storage::StateDir;

pub const DESCRIPTION: &str = "Generate a raster image from a text prompt and save it as a PNG. Use for AI-created bitmap visuals: illustrations, textures, sprites, photos, and mockups. Requires a ChatGPT subscription login (`caudra auth login openai`) and bills against that plan, not API credits.

- Do not use when the asset is better authored directly as SVG, HTML/CSS, or canvas, or when extending an existing icon or logo system.
- Returns the saved path only. Call `view_image` on it when you need to see the result.
- One image per call. For several distinct assets, call once per asset.
- Reference images are passed with `images` and are described in prompt order; label their roles in `prompt`, e.g. \"Image 1: style reference\".
- Never overwrites: an existing `out` path is versioned to `-v2`, `-v3`, and so on.";

const MAX_VERSION: u32 = 999;
const PNG_EXTENSION: &str = "png";
const SAVED: &str = "Generated image saved to";
const VIEW_HINT: &str = "Call view_image on it to see the result.";

static PROMPT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Description of the image to generate.",
};
static OUT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Output file path, relative to the project directory unless absolute. Written as a PNG.",
};
static QUALITY_PARAM: ParamSchema = ParamSchema::Enum {
    variants: &["low", "medium", "high", "auto"],
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
    let out = resolve_path(required_str(&input, "out")?).map_err(ParseError::custom)?;

    let quality = match input.get("quality").and_then(Value::as_str) {
        Some(raw) => ImageQuality::from_wire(raw)
            .ok_or_else(|| ParseError::custom(format!("unknown quality '{raw}'")))?,
        None => ImageQuality::default(),
    };

    let references = match input.get("images").and_then(Value::as_array) {
        Some(paths) => paths
            .iter()
            .map(|path| {
                let raw = path
                    .as_str()
                    .ok_or_else(|| ParseError::custom("images entries must be strings"))?;
                resolve_path(raw).map_err(ParseError::custom)
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };

    Ok(ImageGenerateCall {
        prompt,
        out,
        quality,
        size: input
            .get("size")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        references,
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
    out: String,
    quality: ImageQuality,
    size: Option<String>,
    references: Vec<String>,
}

impl ToolInvocation for ImageGenerateCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(relative_path(&self.out)))
    }

    fn start_annotation(&self) -> Option<String> {
        Some(self.prompt.clone())
    }

    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.out))
    }

    fn read_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
        self.references.iter().map(PathBuf::from).collect()
    }

    fn plan_mode_access(&self) -> PlanModeAccess {
        PlanModeAccess::Refused
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(std::future::ready(Some(PermissionScopes::single(
            self.out.clone(),
        ))))
    }

    fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            match self.run().await {
                Ok(output) => ToolExecResult::from(Ok(output)),
                Err(message) => ToolExecResult::from(Err(message)),
            }
        })
    }
}

impl ImageGenerateCall {
    async fn run(self) -> Result<ToolOutput, String> {
        let paths = self.references.clone();
        let references = smol::unblock(move || read_references(&paths)).await?;
        let state_dir = StateDir::resolve().map_err(|e| e.to_string())?;

        let bytes = generate(
            &state_dir,
            &ImageRequest {
                prompt: &self.prompt,
                quality: self.quality,
                size: self.size.as_deref(),
                references: &references,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

        let requested = self.out.clone();
        smol::unblock(move || save(&requested, &bytes)).await
    }
}

fn read_references(paths: &[String]) -> Result<Vec<ImageSource>, String> {
    paths
        .iter()
        .map(|path| prepare(path).map(|image| image.source))
        .collect()
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
}
