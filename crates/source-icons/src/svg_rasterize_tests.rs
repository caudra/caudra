use std::sync::Arc;

use crate::svg_rasterize::{SafeSvg, SvgRasterizeError, isolated_options};

#[test]
fn resource_resolvers_deny_data_and_string_hrefs() {
    let options = isolated_options();
    assert!(
        (options.image_href_resolver.resolve_string)(
            "file:///tmp/should-not-be-read.png",
            &options,
        )
        .is_none()
    );
    assert!(
        (options.image_href_resolver.resolve_data)(
            "image/png",
            Arc::new(vec![0x89, b'P', b'N', b'G']),
            &options,
        )
        .is_none()
    );
}

#[test]
fn external_images_are_ignored_without_affecting_local_shapes() {
    let svg = SafeSvg::parse(
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8">
            <rect width="8" height="8" fill="#00ff00"/>
            <image width="8" height="8" href="https://127.0.0.1/never-requested.png"/>
        </svg>"##,
    )
    .unwrap();
    let image = svg.rasterize(8).unwrap().to_rgba8();
    assert_eq!(image.get_pixel(4, 4).0, [0, 255, 0, 255]);
}

#[test]
fn document_types_are_rejected_before_parsing() {
    assert!(matches!(
        SafeSvg::parse(
            br#"<!DOCTYPE svg SYSTEM "file:///tmp/never-read.dtd">
                <svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#,
        ),
        Err(SvgRasterizeError::DocumentType)
    ));
}

#[test]
fn large_isolated_render_is_rejected_by_the_pixmap_budget() {
    let svg = SafeSvg::parse(
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="4096" height="4096">
            <g opacity="0.5"><rect width="4096" height="4096" fill="red"/></g>
        </svg>"#,
    )
    .unwrap();
    assert!(matches!(
        svg.rasterize(4096),
        Err(SvgRasterizeError::Allocation)
    ));
}
