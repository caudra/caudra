use image::{DynamicImage, RgbaImage};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Group, ImageHrefResolver, Node, Options, Tree};
use thiserror::Error;

const MAX_DIMENSION: u32 = 4_096;
const MAX_PIXELS: u64 = 4_096 * 4_096;
const MAX_TOTAL_PIXMAP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TREE_NODES: usize = 10_000;
const MAX_TREE_DEPTH: usize = 64;
// resvg clips isolated intermediate layers to a 5x-by-5x canvas rectangle.
const MAX_LAYER_AREA_MULTIPLIER: u64 = 25;
// Masks/clips and one filter primitive can each retain multiple layer-sized buffers.
const AUXILIARY_BUFFERS_PER_LAYER: usize = 3;

/// A parsed SVG whose resource and allocation behavior is fixed by this module.
pub(crate) struct SafeSvg {
    tree: Tree,
    width: f32,
    height: f32,
    allocation_layers: usize,
}

#[derive(Debug, Error)]
pub(crate) enum SvgRasterizeError {
    #[error("SVG contains a disallowed document type declaration")]
    DocumentType,
    #[error("SVG parsing failed: {0}")]
    Parse(#[from] resvg::usvg::Error),
    #[error("SVG dimensions exceed the decode limit")]
    Dimensions,
    #[error("SVG tree exceeds the complexity limit")]
    Complexity,
    #[error("SVG uses a feature outside the bounded rendering profile: {0}")]
    UnsupportedFeature(&'static str),
    #[error("SVG pixmap would exceed the allocation limit")]
    Allocation,
}

impl SafeSvg {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, SvgRasterizeError> {
        // usvg never performs network I/O, but it permits DTD parsing. Favicons
        // need no entities, so reject DTDs before XML parsing to remove entity
        // expansion and external-identifier ambiguity from this trust boundary.
        if contains_ascii_case_insensitive(bytes, b"<!doctype") {
            return Err(SvgRasterizeError::DocumentType);
        }

        let options = isolated_options();
        let tree = Tree::from_data(bytes, &options)?;
        let width = tree.size().width();
        let height = tree.size().height();
        validate_intrinsic_dimensions(width, height)?;
        // Pattern tile pixmaps are sized independently from the output canvas.
        // Reject them rather than trying to infer a safe bound through arbitrary
        // pattern transforms. Solid colors and gradients remain supported.
        if !tree.patterns().is_empty() {
            return Err(SvgRasterizeError::UnsupportedFeature("pattern paint"));
        }
        let stats = analyze_tree(tree.root())?;
        let filter_layers = tree.filters().iter().try_fold(0_usize, |total, filter| {
            total.checked_add(
                filter
                    .primitives()
                    .len()
                    .saturating_mul(AUXILIARY_BUFFERS_PER_LAYER),
            )
        });
        let allocation_layers = stats
            .max_allocation_layer_depth
            .checked_add(filter_layers.ok_or(SvgRasterizeError::Complexity)?)
            .ok_or(SvgRasterizeError::Complexity)?;
        Ok(Self {
            tree,
            width,
            height,
            allocation_layers,
        })
    }

    pub(crate) fn rasterize(&self, requested_size: u32) -> Result<DynamicImage, SvgRasterizeError> {
        let requested_size = requested_size.max(1);
        let scale = (requested_size as f32 / self.width)
            .min(requested_size as f32 / self.height)
            .min(1.0);
        let width = scaled_dimension(self.width, scale, requested_size);
        let height = scaled_dimension(self.height, scale, requested_size);
        validate_pixmap_budget(width, height, self.allocation_layers)?;

        // The only raster target is this bounded in-memory pixmap. resvg's
        // intermediate layers are bounded relative to it, and the budget above
        // accounts for the deepest simultaneously isolated group stack.
        let mut pixmap = Pixmap::new(width, height).ok_or(SvgRasterizeError::Allocation)?;
        resvg::render(
            &self.tree,
            Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let pixels = pixmap.take_demultiplied();
        let image =
            RgbaImage::from_raw(width, height, pixels).ok_or(SvgRasterizeError::Allocation)?;
        Ok(DynamicImage::ImageRgba8(image))
    }
}

pub(crate) fn isolated_options() -> Options<'static> {
    // resvg is compiled without text, system-font, memmap-font, or raster-image
    // features. These resolvers additionally deny data URLs and every string
    // href, so parsing cannot read files or delegate nested image allocation.
    Options {
        resources_dir: None,
        style_sheet: None,
        image_href_resolver: ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Options::default()
    }
}

fn validate_intrinsic_dimensions(width: f32, height: f32) -> Result<(), SvgRasterizeError> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(SvgRasterizeError::Dimensions);
    }
    let width = width.ceil() as u64;
    let height = height.ceil() as u64;
    if width > u64::from(MAX_DIMENSION)
        || height > u64::from(MAX_DIMENSION)
        || width.saturating_mul(height) > MAX_PIXELS
    {
        Err(SvgRasterizeError::Dimensions)
    } else {
        Ok(())
    }
}

fn scaled_dimension(value: f32, scale: f32, requested_size: u32) -> u32 {
    (value * scale).ceil().clamp(1.0, requested_size as f32) as u32
}

fn validate_pixmap_budget(
    width: u32,
    height: u32,
    allocation_layers: usize,
) -> Result<(), SvgRasterizeError> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    let allocation_layers = u64::try_from(allocation_layers).unwrap_or(u64::MAX);
    // The output pixmap costs four bytes per pixel. Each simultaneously nested
    // isolated or auxiliary buffer can use up to 25x the canvas area in resvg
    // 0.47. Filter primitive buffers are also included in `allocation_layers`.
    let allocation_factor =
        1_u64.saturating_add(allocation_layers.saturating_mul(MAX_LAYER_AREA_MULTIPLIER));
    let bytes = pixels.saturating_mul(4).saturating_mul(allocation_factor);
    if pixels > MAX_PIXELS || bytes > MAX_TOTAL_PIXMAP_BYTES {
        Err(SvgRasterizeError::Allocation)
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct TreeStats {
    nodes: usize,
    max_allocation_layer_depth: usize,
}

fn analyze_tree(root: &Group) -> Result<TreeStats, SvgRasterizeError> {
    let mut stats = TreeStats::default();
    analyze_group(root, 0, 0, &mut stats)?;
    Ok(stats)
}

fn analyze_group(
    group: &Group,
    depth: usize,
    allocation_layer_depth: usize,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(SvgRasterizeError::Complexity);
    }
    for node in group.children() {
        stats.nodes = stats.nodes.saturating_add(1);
        if stats.nodes > MAX_TREE_NODES {
            return Err(SvgRasterizeError::Complexity);
        }
        if let Node::Group(group) = node {
            let next_layer_depth =
                allocation_layer_depth.saturating_add(usize::from(group.should_isolate()));
            stats.max_allocation_layer_depth =
                stats.max_allocation_layer_depth.max(next_layer_depth);
            analyze_group(group, depth + 1, next_layer_depth, stats)?;
        }

        let mut subroot_result = Ok(());
        node.subroots(|subroot| {
            if subroot_result.is_ok() {
                // Clip and mask subroots can retain a pixmap, alpha mask, and
                // destination layer concurrently while their children render.
                let subroot_layer_depth =
                    allocation_layer_depth.saturating_add(AUXILIARY_BUFFERS_PER_LAYER);
                stats.max_allocation_layer_depth =
                    stats.max_allocation_layer_depth.max(subroot_layer_depth);
                subroot_result = analyze_group(subroot, depth + 1, subroot_layer_depth, stats);
            }
        });
        subroot_result?;
    }
    Ok(())
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}
