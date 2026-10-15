use image::{DynamicImage, RgbaImage};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::filter::Kind;
use resvg::usvg::{ClipPath, Group, ImageHrefResolver, Mask, Node, Options, Paint, Tree};
use thiserror::Error;

const MAX_DIMENSION: u32 = 4_096;
const MAX_PIXELS: u64 = 4_096 * 4_096;
const MAX_TOTAL_PIXMAP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TREE_NODES: usize = 10_000;
const MAX_TREE_DEPTH: usize = 64;
// resvg clips isolated intermediate layers to a 5x-by-5x canvas rectangle.
const MAX_LAYER_AREA_MULTIPLIER: u64 = 25;
// Masks and clips can retain multiple layer-sized buffers.
const MASK_CLIP_BUFFERS: usize = 3;
const FILTER_PRIMITIVE_BUFFERS: usize = 5;

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
        Ok(Self {
            tree,
            width,
            height,
            allocation_layers: stats.max_allocation_layer_depth,
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
    // 0.48.1. Filter primitive buffers are also included in `allocation_layers`.
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

impl TreeStats {
    fn record(
        &mut self,
        depth: usize,
        allocation_layer_depth: usize,
    ) -> Result<(), SvgRasterizeError> {
        self.nodes = self.nodes.saturating_add(1);
        if depth > MAX_TREE_DEPTH || self.nodes > MAX_TREE_NODES {
            return Err(SvgRasterizeError::Complexity);
        }
        self.max_allocation_layer_depth =
            self.max_allocation_layer_depth.max(allocation_layer_depth);
        Ok(())
    }
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
    stats.record(depth, allocation_layer_depth)?;
    for node in group.children() {
        match node {
            Node::Group(group) => {
                analyze_render_group(group, depth + 1, allocation_layer_depth, stats)?;
            }
            Node::Text(text) => {
                analyze_render_group(text.flattened(), depth + 1, allocation_layer_depth, stats)?;
            }
            Node::Path(path) => {
                stats.record(depth + 1, allocation_layer_depth)?;
                if matches!(
                    path.fill().map(|fill| fill.paint()),
                    Some(Paint::Pattern(_))
                ) || matches!(
                    path.stroke().map(|stroke| stroke.paint()),
                    Some(Paint::Pattern(_))
                ) {
                    return Err(SvgRasterizeError::UnsupportedFeature("pattern paint"));
                }
            }
            Node::Image(_) => {
                return Err(SvgRasterizeError::UnsupportedFeature("image"));
            }
        }
    }
    Ok(())
}

fn analyze_render_group(
    group: &Group,
    depth: usize,
    allocation_layer_depth: usize,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    let layers = allocation_layer_depth.saturating_add(usize::from(group.should_isolate()));
    analyze_group(group, depth, layers, stats)?;

    if let Some(clip) = group.clip_path() {
        analyze_clip(clip, depth + 1, layers, stats)?;
    }
    if let Some(mask) = group.mask() {
        analyze_mask(mask, depth + 1, layers, stats)?;
    }
    for filter in group.filters() {
        for (retained_results, primitive) in filter.primitives().iter().enumerate() {
            let retained_layers = layers.saturating_add(retained_results);
            stats.record(
                depth + 1,
                retained_layers.saturating_add(FILTER_PRIMITIVE_BUFFERS),
            )?;
            if let Kind::Image(image) = primitive.kind() {
                analyze_group(
                    image.root(),
                    depth + 1,
                    retained_layers.saturating_add(1),
                    stats,
                )?;
            }
        }
    }
    Ok(())
}

fn analyze_clip(
    clip: &ClipPath,
    depth: usize,
    allocation_layer_depth: usize,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    let layers = allocation_layer_depth.saturating_add(MASK_CLIP_BUFFERS);
    analyze_group(clip.root(), depth, layers, stats)?;
    if let Some(linked) = clip.clip_path() {
        analyze_clip(linked, depth + 1, layers, stats)?;
    }
    Ok(())
}

fn analyze_mask(
    mask: &Mask,
    depth: usize,
    allocation_layer_depth: usize,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    let layers = allocation_layer_depth.saturating_add(MASK_CLIP_BUFFERS);
    analyze_group(mask.root(), depth, layers, stats)?;
    if let Some(linked) = mask.mask() {
        analyze_mask(linked, depth + 1, layers, stats)?;
    }
    Ok(())
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}
