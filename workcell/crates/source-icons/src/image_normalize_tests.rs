use std::io::Cursor;

use base64::Engine;
use image::{DynamicImage, ImageFormat};

use crate::image_normalize::{
    IconKind, NormalizeError, OutputProfile, normalize_to_data_url, sniff_icon_kind,
};
use crate::svg_rasterize::SvgRasterizeError;

fn encoded_test_image(format: ImageFormat) -> Vec<u8> {
    let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        64,
        32,
        image::Rgba([200, 20, 30, 255]),
    ));
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, format).unwrap();
    output.into_inner()
}

#[test]
fn sniffs_all_supported_and_recognized_formats() {
    assert_eq!(
        sniff_icon_kind(b"\x89PNG\r\n\x1a\nrest"),
        Some(IconKind::Png)
    );
    assert_eq!(sniff_icon_kind(b"\xff\xd8\xffrest"), Some(IconKind::Jpeg));
    assert_eq!(sniff_icon_kind(b"GIF89arest"), Some(IconKind::Gif));
    assert_eq!(
        sniff_icon_kind(b"RIFF\0\0\0\0WEBPrest"),
        Some(IconKind::WebP)
    );
    assert_eq!(sniff_icon_kind(b"\0\0\x01\0rest"), Some(IconKind::Ico));
    assert_eq!(
        sniff_icon_kind(b" <?xml version='1.0'?><SVG width='1'>"),
        Some(IconKind::Svg)
    );
    assert_eq!(sniff_icon_kind(b"<html>not an icon</html>"), None);
}

#[test]
fn valid_bounded_svg_is_normalized_to_png() {
    let output = normalize_to_data_url(
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="16">
            <rect width="32" height="16" fill="#ef3340"/>
        </svg>"##,
        &OutputProfile {
            sizes: vec![24],
            qualities: vec![90],
            soft_limit: usize::MAX,
        },
        1_000_000,
    )
    .unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(output.trim_start_matches("data:image/png;base64,"))
        .unwrap();
    let image = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
    assert_eq!((image.width(), image.height()), (24, 12));
    assert_eq!(image.to_rgba8().get_pixel(12, 6).0, [239, 51, 64, 255]);
}

#[test]
fn malformed_oversized_and_over_byte_limit_svg_fail_safely() {
    let profile = OutputProfile {
        sizes: vec![24],
        qualities: vec![90],
        soft_limit: usize::MAX,
    };
    assert!(matches!(
        normalize_to_data_url(b"<svg><", &profile, 1_000_000),
        Err(NormalizeError::Svg(_))
    ));
    assert!(matches!(
        normalize_to_data_url(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="4097" height="1"/>"#,
            &profile,
            1_000_000,
        ),
        Err(NormalizeError::Svg(SvgRasterizeError::Dimensions))
    ));
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#;
    assert!(matches!(
        normalize_to_data_url(svg, &profile, svg.len() - 1),
        Err(NormalizeError::InputTooLarge)
    ));
}

#[test]
fn decodes_and_normalizes_every_raster_magic_format() {
    let cases = [
        (ImageFormat::Png, IconKind::Png),
        (ImageFormat::Jpeg, IconKind::Jpeg),
        (ImageFormat::Gif, IconKind::Gif),
        (ImageFormat::WebP, IconKind::WebP),
        (ImageFormat::Ico, IconKind::Ico),
    ];
    for (format, expected_kind) in cases {
        let bytes = encoded_test_image(format);
        assert_eq!(sniff_icon_kind(&bytes), Some(expected_kind), "{format:?}");
        let output = normalize_to_data_url(
            &bytes,
            &OutputProfile {
                sizes: vec![24],
                qualities: vec![90],
                soft_limit: usize::MAX,
            },
            bytes.len(),
        )
        .unwrap();
        assert!(output.starts_with("data:image/png;base64,"), "{format:?}");
    }
}

#[test]
fn output_size_ladder_resizes_without_enlargement() {
    let source = encoded_test_image(ImageFormat::Png);
    let output = normalize_to_data_url(
        &source,
        &OutputProfile {
            sizes: vec![24, 16],
            qualities: vec![90],
            soft_limit: usize::MAX,
        },
        source.len(),
    )
    .unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(output.trim_start_matches("data:image/png;base64,"))
        .unwrap();
    let image = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
    assert_eq!((image.width(), image.height()), (24, 12));

    let tiny = DynamicImage::new_rgba8(8, 4);
    let mut encoded = Cursor::new(Vec::new());
    tiny.write_to(&mut encoded, ImageFormat::Png).unwrap();
    let output = normalize_to_data_url(
        encoded.get_ref(),
        &OutputProfile {
            sizes: vec![24],
            qualities: vec![90],
            soft_limit: usize::MAX,
        },
        encoded.get_ref().len(),
    )
    .unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(output.trim_start_matches("data:image/png;base64,"))
        .unwrap();
    let image = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
    assert_eq!((image.width(), image.height()), (8, 4));
}
