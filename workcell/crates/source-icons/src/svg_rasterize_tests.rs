use std::sync::Arc;

use crate::svg_rasterize::{SafeSvg, SvgRasterizeError, isolated_options};

const DROP_SHADOW: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="400">
    <defs><filter id="f" filterUnits="userSpaceOnUse" x="-800" y="-800" width="2000" height="2000">
        <feDropShadow stdDeviation="1"/>
    </filter></defs><rect width="400" height="400" fill="red" filter="url(#f)"/>
</svg>"##;
const FLOOD_FILTER: &str = r#"<filter id="f" filterUnits="userSpaceOnUse"
    x="-100000000" y="-100000000" width="200000000" height="200000000">
    <feFlood flood-color="red"/>
</filter>"#;
const NEAR_SINGULAR_TRANSFORM: &str = "matrix(1 0 1 0.000001 0 0)";

fn filtered_svg(content: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="400">
            <defs>{FLOOD_FILTER}</defs>{content}</svg>"#,
    )
}

fn linked_resources(kind: &str, count: usize, last_content: &str) -> String {
    let (element, attribute) = match kind {
        "mask" => ("mask", "mask"),
        "clip" => ("clipPath", "clip-path"),
        _ => unreachable!(),
    };
    let mut svg =
        String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="400"><defs>"#);
    for index in 0..count {
        let link = if index + 1 < count {
            format!(r#" {attribute}="url(#r{})""#, index + 1)
        } else {
            String::new()
        };
        let content = if index + 1 == count {
            last_content
        } else {
            r#"<rect width="400" height="400" fill="white"/>"#
        };
        svg.push_str(&format!(
            r#"<{element} id="r{index}"{link}>{content}</{element}>"#
        ));
    }
    svg.push_str(&format!(
        r#"</defs><rect width="400" height="400" fill="red" {attribute}="url(#r0)"/></svg>"#
    ));
    svg
}

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

#[test]
fn drop_shadow_iir_scratch_is_included_in_the_budget() {
    let svg = SafeSvg::parse(DROP_SHADOW).unwrap();
    assert!(matches!(
        svg.rasterize(400),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn drop_shadow_budget_accepts_the_last_bounded_size_only() {
    let svg = SafeSvg::parse(DROP_SHADOW).unwrap();
    assert_eq!(svg.rasterize(308).unwrap().width(), 308);
    assert!(matches!(
        svg.rasterize(309),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn linked_resources_preserve_chain_depth() {
    for kind in ["mask", "clip"] {
        let xml = linked_resources(kind, 66, r#"<rect width="400" height="400" fill="white"/>"#);
        assert!(
            matches!(
                SafeSvg::parse(xml.as_bytes()),
                Err(SvgRasterizeError::Complexity)
            ),
            "{kind}"
        );
    }
}

#[test]
fn linked_resources_retain_buffers_but_small_chains_still_render() {
    for kind in ["mask", "clip"] {
        let xml = linked_resources(kind, 6, r#"<rect width="400" height="400" fill="white"/>"#);
        let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
        assert!(
            matches!(svg.rasterize(400), Err(SvgRasterizeError::Allocation)),
            "{kind}"
        );
        assert_eq!(
            svg.rasterize(16).unwrap().to_rgba8().get_pixel(8, 8).0,
            [255, 0, 0, 255],
            "{kind}"
        );
    }
}

#[test]
fn third_link_nodes_are_included_in_the_complexity_limit() {
    let content = r#"<rect width="400" height="400" fill="white"/>"#.repeat(10_001);
    for kind in ["mask", "clip"] {
        let xml = linked_resources(kind, 3, &content);
        assert!(
            matches!(
                SafeSvg::parse(xml.as_bytes()),
                Err(SvgRasterizeError::Complexity)
            ),
            "{kind}"
        );
    }
}

#[test]
fn patterns_in_the_third_link_are_rejected() {
    let xml = linked_resources(
        "mask",
        3,
        r##"
        <defs><pattern id="p" width="1" height="1" patternUnits="userSpaceOnUse">
            <rect width="1" height="1" fill="white"/>
        </pattern></defs><rect width="400" height="400" fill="url(#p)"/>"##,
    );
    assert!(matches!(
        SafeSvg::parse(xml.as_bytes()),
        Err(SvgRasterizeError::UnsupportedFeature("pattern paint"))
    ));
}

#[test]
fn filters_in_the_third_mask_link_are_budgeted() {
    let xml = linked_resources(
        "mask",
        3,
        r##"
        <defs><filter id="f"><feDropShadow stdDeviation="1"/></filter></defs>
        <rect width="400" height="400" fill="white" filter="url(#f)"/>"##,
    );
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert!(matches!(
        svg.rasterize(400),
        Err(SvgRasterizeError::Allocation)
    ));
    assert_eq!(svg.rasterize(16).unwrap().width(), 16);
}

#[test]
fn earlier_filter_results_remain_budgeted_during_drop_shadow() {
    let xml = String::from_utf8(DROP_SHADOW.to_vec()).unwrap().replace(
        "<feDropShadow",
        r#"<feFlood/><feFlood/><feFlood/><feFlood/><feDropShadow"#,
    );
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert_eq!(svg.rasterize(246).unwrap().width(), 246);
    assert!(matches!(
        svg.rasterize(247),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn local_filter_images_retain_outer_results_and_nested_layers() {
    let xml = br##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="400">
        <defs>
            <filter id="inner"><feDropShadow stdDeviation="1"/></filter>
            <g id="fragment" filter="url(#inner)"><rect width="400" height="400" fill="red"/></g>
            <filter id="outer" filterUnits="userSpaceOnUse" x="-800" y="-800" width="2000" height="2000">
                <feFlood/><feImage href="#fragment" x="0" y="0" width="400" height="400"/>
            </filter>
        </defs><rect width="400" height="400" filter="url(#outer)"/>
    </svg>"##;
    let svg = SafeSvg::parse(xml).unwrap();
    assert_eq!(svg.rasterize(246).unwrap().width(), 246);
    assert!(matches!(
        svg.rasterize(247),
        Err(SvgRasterizeError::Allocation)
    ));
    assert!(
        svg.rasterize(16)
            .unwrap()
            .to_rgba8()
            .pixels()
            .any(|pixel| pixel.0[0] > 0)
    );
}

#[test]
fn ill_conditioned_filter_transforms_are_rejected_before_rendering() {
    for content in [
        format!(
            r##"<rect width="400" height="400" transform="{NEAR_SINGULAR_TRANSFORM}" filter="url(#f)"/>"##,
        ),
        format!(
            r##"<g transform="{NEAR_SINGULAR_TRANSFORM}"><rect width="400" height="400" filter="url(#f)"/></g>"##,
        ),
        r##"<rect width="400" height="400" transform="matrix(0.000001 1 0 1 0 0)" filter="url(#f)"/>"##.into(),
    ] {
        let xml = filtered_svg(&content);
        let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
        assert!(matches!(svg.rasterize(16), Err(SvgRasterizeError::Allocation)));
    }
}

#[test]
fn nested_filter_rotations_accumulate_context_expansion() {
    let single = filtered_svg(
        r##"<g transform="rotate(45)" filter="url(#f)"><rect width="400" height="400"/></g>"##,
    );
    assert_eq!(
        SafeSvg::parse(single.as_bytes())
            .unwrap()
            .rasterize(160)
            .unwrap()
            .to_rgba8()
            .get_pixel(80, 80)
            .0,
        [255, 0, 0, 255],
    );
    let xml = filtered_svg(
        r##"<g transform="rotate(45)" filter="url(#f)">
            <g transform="rotate(45)" filter="url(#f)"><rect width="400" height="400"/></g>
        </g>"##,
    );
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert!(matches!(
        svg.rasterize(160),
        Err(SvgRasterizeError::Allocation)
    ));
    assert_eq!(
        svg.rasterize(16).unwrap().to_rgba8().get_pixel(8, 8).0,
        [255, 0, 0, 255]
    );
}

#[test]
fn rotated_filters_on_thin_canvases_are_budgeted_by_bounding_area() {
    let xml = filtered_svg(
        r##"<rect width="4096" height="1" transform="rotate(45)" filter="url(#f)"/>"##,
    )
    .replace(r#"width="400" height="400""#, r#"width="4096" height="1""#);
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert!(matches!(
        svg.rasterize(4096),
        Err(SvgRasterizeError::Allocation)
    ));
    assert_eq!(
        svg.rasterize(16).unwrap().to_rgba8().get_pixel(8, 0).0,
        [255, 0, 0, 255]
    );
}

#[test]
fn differently_sized_filters_retain_a_cropped_source_buffer() {
    let xml = String::from_utf8(DROP_SHADOW.to_vec()).unwrap()
        .replace(
            "</defs>",
            r#"<filter id="smaller" filterUnits="userSpaceOnUse" x="-600" y="-600" width="1600" height="1600">
                <feDropShadow stdDeviation="1"/>
            </filter></defs>"#,
        )
        .replace(r##"filter="url(#f)""##, r##"filter="url(#f) url(#smaller)""##);
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert_eq!(svg.rasterize(308).unwrap().width(), 308);
    assert!(matches!(
        svg.rasterize(309),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn filters_in_linked_masks_inherit_the_render_transform() {
    let xml = linked_resources(
        "mask",
        3,
        &format!(
            r##"<defs>{FLOOD_FILTER}</defs><rect width="400" height="400" filter="url(#f)"/>"##,
        ),
    )
    .replace(
        r#"fill="red" mask="url(#r0)""#,
        &format!(r#"fill="red" mask="url(#r0)" transform="{NEAR_SINGULAR_TRANSFORM}""#),
    );
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert!(matches!(
        svg.rasterize(16),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn linked_auxiliary_buffers_use_the_expanded_filter_context() {
    for kind in ["mask", "clip"] {
        let xml = linked_resources(kind, 3, r#"<rect width="400" height="400" fill="white"/>"#)
            .replace("<defs>", &format!("<defs>{FLOOD_FILTER}"))
            .replace(
                r#"fill="red""#,
                r##"fill="red" transform="rotate(45 200 200)" filter="url(#f)""##,
            );
        let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
        assert!(
            matches!(svg.rasterize(192), Err(SvgRasterizeError::Allocation)),
            "{kind}"
        );
        assert_eq!(
            svg.rasterize(16).unwrap().to_rgba8().get_pixel(8, 8).0,
            [255, 0, 0, 255],
            "{kind}"
        );
    }
}

#[test]
fn local_filter_images_budget_their_own_transformed_filters() {
    let xml = filtered_svg(&format!(
        r##"<defs>
            <g id="fragment" transform="{NEAR_SINGULAR_TRANSFORM}" filter="url(#f)">
                <rect width="400" height="400"/>
            </g>
            <filter id="outer"><feFlood/><feImage href="#fragment"/></filter>
        </defs><rect width="400" height="400" filter="url(#outer)"/>"##,
    ));
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert!(matches!(
        svg.rasterize(16),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn bounded_filter_transforms_still_render() {
    for transform in [
        "rotate(45 200 200)",
        "skewX(30)",
        "scale(0.5 2)",
        "matrix(-1 0 0 1 400 0)",
    ] {
        let xml = filtered_svg(&format!(
            r##"<rect width="400" height="400" transform="{transform}" filter="url(#f)"/>"##,
        ));
        let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
        assert_eq!(
            svg.rasterize(16).unwrap().to_rgba8().get_pixel(8, 8).0,
            [255, 0, 0, 255],
            "{transform}"
        );
    }
}

#[test]
fn unfiltered_ill_conditioned_transforms_do_not_expand_the_budget() {
    let xml = filtered_svg(&format!(
        r#"<rect width="400" height="400" fill="red"/>
            <g transform="{NEAR_SINGULAR_TRANSFORM}" opacity="0.5">
                <rect width="400" height="400" fill="red"/>
            </g>"#,
    ));
    let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
    assert_eq!(
        svg.rasterize(400).unwrap().to_rgba8().get_pixel(200, 200).0,
        [255, 0, 0, 255]
    );
}

#[test]
fn oversized_filter_primitive_regions_are_rejected_before_rendering() {
    for attributes in [
        r#"width="1e20" height="16""#,
        r#"width="16" height="1e20""#,
        r#"x="2147483520" width="256" height="16""#,
        r#"y="2147483520" width="16" height="256""#,
    ] {
        let xml = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
                <filter id="f" filterUnits="userSpaceOnUse" x="0" y="0" width="16" height="16">
                    <feFlood {attributes}/>
                </filter><rect width="16" height="16" filter="url(#f)"/>
            </svg>"##,
        );
        let svg = SafeSvg::parse(xml.as_bytes()).unwrap();
        assert!(
            matches!(svg.rasterize(16), Err(SvgRasterizeError::Allocation)),
            "{attributes}"
        );
    }
}

#[test]
fn primitive_integer_regions_include_inherited_translations() {
    let xml = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
        <filter id="f" filterUnits="userSpaceOnUse" x="-1024" y="0" width="1040" height="16">
            <feFlood x="2147483008" y="0" width="256" height="16"/>
        </filter>
        <g transform="translate(1024 0)" opacity="0.5">
            <rect x="-1024" width="16" height="16" filter="url(#f)"/>
        </g>
    </svg>"##;
    let svg = SafeSvg::parse(xml).unwrap();
    assert!(matches!(
        svg.rasterize(16),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn oversized_primitive_regions_in_local_filter_images_are_rejected() {
    let xml = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
        <defs>
            <filter id="inner" filterUnits="userSpaceOnUse" x="0" y="0" width="16" height="16">
                <feFlood width="1e20" height="16"/>
            </filter>
            <g id="fragment" filter="url(#inner)"><rect width="16" height="16"/></g>
            <filter id="outer"><feImage href="#fragment" x="0" y="0" width="16" height="16"/></filter>
        </defs><rect width="16" height="16" filter="url(#outer)"/>
    </svg>"##;
    let svg = SafeSvg::parse(xml).unwrap();
    assert!(matches!(
        svg.rasterize(16),
        Err(SvgRasterizeError::Allocation)
    ));
}

#[test]
fn bounded_primitive_regions_may_extend_beyond_the_filter() {
    let xml = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
        <filter id="f" filterUnits="userSpaceOnUse" x="0" y="0" width="16" height="16">
            <feFlood width="1000000" height="16" flood-color="red"/>
        </filter><rect width="16" height="16" filter="url(#f)"/>
    </svg>"##;
    let svg = SafeSvg::parse(xml).unwrap();
    assert_eq!(
        svg.rasterize(16).unwrap().to_rgba8().get_pixel(8, 8).0,
        [255, 0, 0, 255]
    );
}
