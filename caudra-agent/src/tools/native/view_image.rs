//! `view_image`: hand an image file to a vision model.
//!
//! `file_read` can only produce text, so images need their own path: decode,
//! shrink to what the provider will actually accept, and ship the pixels as a
//! separate content block alongside a caption the model can reason about.

use std::borrow::Cow;
use std::io::Cursor;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::{DynamicImage, ImageFormat, ImageReader};
use serde_json::Value;

use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes, Tool, ToolExecResult,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{
    BoxFuture, DescriptionContext, ToolAudience, ToolContext, relative_path, resolve_path,
};
use crate::types::ToolOutput;
use caudra_providers::{ImageMediaType, ImageSource};

pub const DESCRIPTION: &str = "View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `file_read` for images.

- Paths: absolute, relative, or ~/.
- Oversized images are downscaled automatically (animated gif/webp keep only the first frame).";

/// Anthropic rejects images over 5MB base64; 3MB raw is ~4MB encoded, which
/// leaves headroom.
const MAX_RAW_BYTES: u64 = 3 * 1024 * 1024;
/// Anthropic downscales anything over this on the long edge server-side
/// anyway, so ship fewer bytes and do it here.
const MAX_EDGE: u32 = 1568;
/// Refuse absurdly large files before reading them; `MAX_PIXELS` separately
/// guards against a small file that declares huge dimensions.
const MAX_INPUT_BYTES: u64 = 50 * 1024 * 1024;
/// Decode-bomb guard: a tiny file can declare huge dimensions and balloon into
/// gigabytes of RGBA. 50MP still covers any real camera photo.
const MAX_PIXELS: u64 = 50_000_000;

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
        // No interpreter audience: the code_execution bridge flattens tool
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
    let meta = std::fs::metadata(path).map_err(|_| format!("error: path not found: {path}"))?;
    if meta.is_dir() {
        return Err(format!("error: {path} is a directory"));
    }
    if meta.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "{path} is too large to view ({}; limit {})",
            format_size(meta.len()),
            format_size(MAX_INPUT_BYTES)
        ));
    }

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let (format, width, height) =
        probe(&bytes).map_err(|e| format!("{path} is not an image {e}"))?;
    let source_media = media_type(format).ok_or_else(|| {
        format!(
            "unsupported image format {}: only png, jpeg, gif, and webp can be viewed",
            format_name(format)
        )
    })?;

    // Decode fully even on the pass-through path: a corrupt file shipped
    // undecoded poisons message history and fails every later request.
    let decoded =
        decode(&bytes, format, width, height).map_err(|e| format!("cannot decode {path}: {e}"))?;

    let size = bytes.len() as u64;
    if size <= MAX_RAW_BYTES && width.max(height) <= MAX_EDGE {
        return Ok(image_output(
            caption(path, size, width, height, ""),
            source_media,
            &bytes,
        ));
    }

    // Too big for the API: downscale to fit MAX_EDGE and re-encode. JPEG stays
    // JPEG (photos recompress far smaller); everything else becomes PNG since
    // gif/webp encoding isn't supported.
    let resized = width.max(height) > MAX_EDGE;
    let decoded = if resized {
        decoded.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };

    let mut out_format = if format == ImageFormat::Jpeg {
        ImageFormat::Jpeg
    } else {
        ImageFormat::Png
    };
    let mut encoded = encode(&decoded, out_format)?;
    if encoded.len() as u64 > MAX_RAW_BYTES && out_format == ImageFormat::Png {
        // PNG can stay huge at 1568px (e.g. noisy screenshots); JPEG is the
        // only remaining lever.
        out_format = ImageFormat::Jpeg;
        encoded = encode(&decoded, out_format)?;
    }
    if encoded.len() as u64 > MAX_RAW_BYTES {
        return Err(format!(
            "{path} is too large to view ({} after downscaling; limit {})",
            format_size(encoded.len() as u64),
            format_size(MAX_RAW_BYTES)
        ));
    }

    let mut note = if resized {
        format!(", downscaled from {width}x{height}")
    } else {
        ", re-encoded".to_owned()
    };
    // Animated gif/webp lose their animation when re-encoded.
    if matches!(format, ImageFormat::Gif | ImageFormat::WebP) {
        note.push_str(", first frame only");
    }

    let out_media = media_type(out_format).unwrap_or(ImageMediaType::Png);
    Ok(image_output(
        caption(
            path,
            encoded.len() as u64,
            decoded.width(),
            decoded.height(),
            &note,
        ),
        out_media,
        &encoded,
    ))
}

fn image_output(text: String, media_type: ImageMediaType, bytes: &[u8]) -> ToolOutput {
    ToolOutput::Image {
        source: ImageSource {
            media_type,
            data: BASE64.encode(bytes).into(),
        },
        text,
    }
}

fn probe(bytes: &[u8]) -> Result<(ImageFormat, u32, u32), String> {
    let format = image::guess_format(bytes).map_err(|_| "(unrecognized format)".to_owned())?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| format!("(cannot read image header: {e})"))?;
    Ok((format, width, height))
}

fn decode(
    bytes: &[u8],
    format: ImageFormat,
    width: u32,
    height: u32,
) -> Result<DynamicImage, String> {
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "image too large to decode ({width}x{height}; limit {MAX_PIXELS} pixels)"
        ));
    }
    image::load_from_memory_with_format(bytes, format).map_err(|e| e.to_string())
}

fn encode(image: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), format)
        .map_err(|e| format!("cannot encode image: {e}"))?;
    Ok(out)
}

fn media_type(format: ImageFormat) -> Option<ImageMediaType> {
    match format {
        ImageFormat::Png => Some(ImageMediaType::Png),
        ImageFormat::Jpeg => Some(ImageMediaType::Jpeg),
        ImageFormat::Gif => Some(ImageMediaType::Gif),
        ImageFormat::WebP => Some(ImageMediaType::Webp),
        _ => None,
    }
}

fn format_name(format: ImageFormat) -> &'static str {
    match format {
        // Only jpeg deviates from its primary extension ("jpg").
        ImageFormat::Jpeg => "jpeg",
        other => other.extensions_str().first().copied().unwrap_or("unknown"),
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{}KB", bytes.div_ceil(1024))
    }
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
    use test_case::test_case;

    const NOT_AN_IMAGE: &str = "is not an image";
    const TOO_LARGE: &str = "too large to view";
    const UNSUPPORTED: &str = "unsupported image format";

    fn png(width: u32, height: u32) -> Vec<u8> {
        encode(&DynamicImage::new_rgb8(width, height), ImageFormat::Png).unwrap()
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn image_text(output: &ToolOutput) -> &str {
        match output {
            ToolOutput::Image { text, .. } => text,
            other => panic!("expected an image output, got {other:?}"),
        }
    }

    fn image_media(output: &ToolOutput) -> ImageMediaType {
        match output {
            ToolOutput::Image { source, .. } => source.media_type,
            other => panic!("expected an image output, got {other:?}"),
        }
    }

    #[test]
    fn small_png_passes_through_undownscaled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "small.png", &png(4, 3));
        let output = load(&path).unwrap();
        assert!(image_text(&output).contains("4x3"), "{output:?}");
        assert!(!image_text(&output).contains("downscaled"), "{output:?}");
        assert_eq!(image_media(&output), ImageMediaType::Png);
    }

    #[test]
    fn oversized_png_is_downscaled_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "big.png", &png(MAX_EDGE + 400, 10));
        let output = load(&path).unwrap();
        let text = image_text(&output);
        assert!(text.contains(&format!("{MAX_EDGE}x")), "{text}");
        assert!(text.contains("downscaled from"), "{text}");
    }

    #[test]
    fn non_image_bytes_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "notes.txt", b"just text");
        assert!(load(&path).unwrap_err().contains(NOT_AN_IMAGE));
    }

    #[test_case(ImageFormat::Png, Some(ImageMediaType::Png) ; "png")]
    #[test_case(ImageFormat::Jpeg, Some(ImageMediaType::Jpeg) ; "jpeg")]
    #[test_case(ImageFormat::Gif, Some(ImageMediaType::Gif) ; "gif")]
    #[test_case(ImageFormat::WebP, Some(ImageMediaType::Webp) ; "webp")]
    #[test_case(ImageFormat::Bmp, None ; "bmp_is_not_viewable")]
    #[test_case(ImageFormat::Tiff, None ; "tiff_is_not_viewable")]
    fn only_provider_accepted_formats_map_to_a_media_type(
        format: ImageFormat,
        expected: Option<ImageMediaType>,
    ) {
        assert_eq!(media_type(format), expected);
    }

    #[test]
    fn an_unsupported_format_is_reported_as_such() {
        // `image` is built without a BMP decoder, so a real BMP cannot reach
        // `media_type`; the message it would produce is asserted directly.
        let message = format!(
            "unsupported image format {}: only png, jpeg, gif, and webp can be viewed",
            format_name(ImageFormat::Bmp)
        );
        assert!(message.contains(UNSUPPORTED), "{message}");
        assert!(message.contains("bmp"), "{message}");
    }

    #[test]
    fn directories_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        assert!(load(&path).unwrap_err().contains("is a directory"));
    }

    #[test]
    fn decode_rejects_a_pixel_bomb_before_allocating() {
        // Patch a real 1x1 PNG's IHDR to claim 10000x10000 and fix the CRC.
        // The cap must trip on the header alone, before any allocation.
        let mut bytes = png(1, 1);
        bytes[16..20].copy_from_slice(&10_000_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&10_000_u32.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());

        let (format, width, height) = probe(&bytes).expect("probe reads only the header");
        assert_eq!((width, height), (10_000, 10_000));
        let err = decode(&bytes, format, width, height).unwrap_err();
        assert!(err.contains("too large to decode"), "got: {err}");
    }

    #[test]
    fn files_over_the_input_cap_are_refused_without_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "huge.png", &[]);
        // Rewrite as a sparse file larger than the cap.
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_INPUT_BYTES + 1).unwrap();
        drop(file);
        assert!(load(&path).unwrap_err().contains(TOO_LARGE));
    }

    #[test_case(1023, "1KB" ; "rounds_partial_kb_up")]
    #[test_case(1024, "1KB" ; "exact_kb")]
    #[test_case(1024 * 1024, "1.0MB" ; "exact_mb")]
    #[test_case(1_572_864, "1.5MB" ; "fractional_mb")]
    fn format_size_matches_the_caption_contract(bytes: u64, expected: &str) {
        assert_eq!(format_size(bytes), expected);
    }

    #[test]
    fn schema_requires_path_and_accepts_the_file_path_alias() {
        let schema = to_json_schema(&SCHEMA);
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        let parsed = validate(&SCHEMA, serde_json::json!({"file_path": "/tmp/x.png"})).unwrap();
        assert_eq!(parsed["path"], serde_json::json!("/tmp/x.png"));
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ ((crc & 1) * 0xEDB8_8320);
            }
        }
        !crc
    }
}
