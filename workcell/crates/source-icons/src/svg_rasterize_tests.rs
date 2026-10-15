use std::sync::Arc;

use crate::svg_rasterize::{SafeSvg, SvgRasterizeError, isolated_options};

const DROP_SHADOW: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="400">
    <defs><filter id="f" filterUnits="userSpaceOnUse" x="-800" y="-800" width="2000" height="2000">
        <feDropShadow stdDeviation="1"/>
    </filter></defs><rect width="400" height="400" fill="red" filter="url(#f)"/>
</svg>"##;

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
    assert_eq!(svg.rasterize(333).unwrap().width(), 333);
    assert!(matches!(
        svg.rasterize(334),
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
    assert_eq!(svg.rasterize(258).unwrap().width(), 258);
    assert!(matches!(
        svg.rasterize(259),
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
    assert_eq!(svg.rasterize(272).unwrap().width(), 272);
    assert!(matches!(
        svg.rasterize(273),
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
