use std::io::Cursor;

use base64::Engine;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{DynamicImage, ImageEncoder, ImageFormat, ImageReader, Limits};
use thiserror::Error;

use crate::svg_rasterize::{SafeSvg, SvgRasterizeError};

const MAX_DIMENSION: u32 = 4_096;
const MAX_PIXELS: u64 = 4_096 * 4_096;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IconKind {
    Png,
    Jpeg,
    Gif,
    WebP,
    Ico,
    Svg,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputProfile {
    pub(crate) sizes: Vec<u32>,
    pub(crate) qualities: Vec<u8>,
    pub(crate) soft_limit: usize,
}

#[derive(Debug, Error)]
pub(crate) enum NormalizeError {
    #[error("unrecognized icon bytes")]
    UnknownFormat,
    #[error("icon exceeds the configured input byte limit")]
    InputTooLarge,
    #[error("image dimensions exceed the decode limit")]
    Dimensions,
    #[error("image decode failed: {0}")]
    Decode(#[from] image::ImageError),
    #[error(transparent)]
    Svg(#[from] SvgRasterizeError),
}

pub(crate) fn sniff_icon_kind(bytes: &[u8]) -> Option<IconKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(IconKind::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(IconKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(IconKind::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(IconKind::WebP)
    } else if bytes.starts_with(&[0, 0, 1, 0]) {
        Some(IconKind::Ico)
    } else if looks_like_svg(bytes) {
        Some(IconKind::Svg)
    } else {
        None
    }
}

pub(crate) fn normalize_to_data_url(
    bytes: &[u8],
    profile: &OutputProfile,
    max_input_bytes: usize,
) -> Result<String, NormalizeError> {
    // The resolver already stops the HTTP body at this bound. Rechecking at the
    // CPU boundary preserves that invariant if another internal caller is added.
    if bytes.len() > max_input_bytes {
        return Err(NormalizeError::InputTooLarge);
    }
    let kind = sniff_icon_kind(bytes).ok_or(NormalizeError::UnknownFormat)?;
    let decoded = match kind {
        IconKind::Svg => DecodedIcon::Svg(Box::new(SafeSvg::parse(bytes)?)),
        kind => {
            let format = match kind {
                IconKind::Png => ImageFormat::Png,
                IconKind::Jpeg => ImageFormat::Jpeg,
                IconKind::Gif => ImageFormat::Gif,
                IconKind::WebP => ImageFormat::WebP,
                IconKind::Ico => ImageFormat::Ico,
                IconKind::Svg => unreachable!(),
            };
            let dimensions = dimensions_with_limits(bytes, format)?;
            validate_dimensions(dimensions)?;
            DecodedIcon::Raster(decode_with_limits(bytes, format)?)
        }
    };
    let mut best = None;
    for &size in &profile.sizes {
        let resized = match &decoded {
            DecodedIcon::Raster(image) => resize_without_enlargement(image, size),
            // SVG is rendered directly at each ladder size. It is never retained
            // as active XML or handed to a browser-facing caller.
            DecodedIcon::Svg(svg) => svg.rasterize(size)?,
        };
        for &quality in &profile.qualities {
            let png = encode_png(&resized, quality)?;
            let data_url = format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(png)
            );
            if best
                .as_ref()
                .is_none_or(|current: &String| data_url.len() < current.len())
            {
                best = Some(data_url.clone());
            }
            if data_url.len() <= profile.soft_limit {
                return Ok(data_url);
            }
        }
    }
    best.ok_or(NormalizeError::Dimensions)
}

enum DecodedIcon {
    Raster(DynamicImage),
    Svg(Box<SafeSvg>),
}

fn dimensions_with_limits(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32), NormalizeError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decode_limits());
    Ok(reader.into_dimensions()?)
}

fn decode_with_limits(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, NormalizeError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decode_limits());
    Ok(reader.decode()?)
}

fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

fn validate_dimensions((width, height): (u32, u32)) -> Result<(), NormalizeError> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        Err(NormalizeError::Dimensions)
    } else {
        Ok(())
    }
}

fn resize_without_enlargement(image: &DynamicImage, size: u32) -> DynamicImage {
    if image.width() <= size && image.height() <= size {
        image.clone()
    } else {
        image.thumbnail(size, size)
    }
}

fn encode_png(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, NormalizeError> {
    let rgba = image.to_rgba8();
    let (compression, filter) = if quality >= 90 {
        (CompressionType::Best, FilterType::Adaptive)
    } else if quality >= 75 {
        (CompressionType::Best, FilterType::Paeth)
    } else {
        (CompressionType::Fast, FilterType::Sub)
    };
    let mut output = Vec::new();
    PngEncoder::new_with_quality(&mut output, compression, filter).write_image(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(output)
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(256)];
    let text = String::from_utf8_lossy(prefix);
    let mut text = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    if text
        .get(..5)
        .is_some_and(|start| start.eq_ignore_ascii_case("<?xml"))
    {
        let Some(end) = text.find("?>") else {
            return false;
        };
        text = text[end + 2..].trim_start();
    }
    text.get(..4)
        .is_some_and(|start| start.eq_ignore_ascii_case("<svg"))
        && text
            .as_bytes()
            .get(4)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'>')
}
