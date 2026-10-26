use image::{DynamicImage, RgbaImage};
use resvg::tiny_skia::{IntRect, NonZeroRect, Pixmap, Transform};
use resvg::usvg::filter::Kind;
use resvg::usvg::{ClipPath, Group, ImageHrefResolver, Mask, Node, Options, Paint, Tree};
use thiserror::Error;

const MAX_DIMENSION: u32 = 4_096;
const MAX_PIXELS: u64 = 4_096 * 4_096;
const MAX_TOTAL_PIXMAP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TREE_NODES: usize = 10_000;
const MAX_TREE_DEPTH: usize = 64;
const MAX_LAYER_DIMENSION_MULTIPLIER: u32 = 5;
const MAX_LAYER_ORIGIN_MULTIPLIER: i32 = -2;
// Masks and clips can retain multiple layer-sized buffers.
const MASK_CLIP_BUFFERS: u64 = 3;
const FILTER_PRIMITIVE_BUFFERS: u64 = 5;
const FILTER_SOURCE_BUFFERS: u64 = 1;
const LAYER_ANTIALIAS_PADDING: i32 = 2;

/// A parsed SVG whose resource and allocation behavior is fixed by this module.
pub(crate) struct SafeSvg {
    tree: Tree,
    width: f32,
    height: f32,
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
        analyze_tree(tree.root(), None)?;
        Ok(Self {
            tree,
            width,
            height,
        })
    }

    pub(crate) fn rasterize(&self, requested_size: u32) -> Result<DynamicImage, SvgRasterizeError> {
        let requested_size = requested_size.max(1);
        let scale = (requested_size as f32 / self.width)
            .min(requested_size as f32 / self.height)
            .min(1.0);
        let width = scaled_dimension(self.width, scale, requested_size);
        let height = scaled_dimension(self.height, scale, requested_size);
        let context = RenderContext::new(width, height, scale)?;
        let stats = analyze_tree(self.tree.root(), Some(&context))?;
        validate_pixmap_budget(width, height, stats.max_allocation_pixels)?;

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
    allocation_pixels: u64,
) -> Result<(), SvgRasterizeError> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    let bytes = pixels.saturating_add(allocation_pixels).saturating_mul(4);
    if pixels > MAX_PIXELS || bytes > MAX_TOTAL_PIXMAP_BYTES {
        Err(SvgRasterizeError::Allocation)
    } else {
        Ok(())
    }
}

struct RenderContext {
    transform: Transform,
    max_bbox: IntRect,
    render_bbox: IntRect,
    pixmap_bounds: IntRect,
}

impl RenderContext {
    fn new(width: u32, height: u32, scale: f32) -> Result<Self, SvgRasterizeError> {
        let max_bbox = IntRect::from_xywh(
            (width as i32) * MAX_LAYER_ORIGIN_MULTIPLIER,
            (height as i32) * MAX_LAYER_ORIGIN_MULTIPLIER,
            width * MAX_LAYER_DIMENSION_MULTIPLIER,
            height * MAX_LAYER_DIMENSION_MULTIPLIER,
        )
        .ok_or(SvgRasterizeError::Allocation)?;
        Ok(Self {
            transform: Transform::from_scale(scale, scale),
            max_bbox,
            render_bbox: max_bbox,
            pixmap_bounds: IntRect::from_xywh(0, 0, width, height)
                .ok_or(SvgRasterizeError::Allocation)?,
        })
    }

    fn for_group(&self, group: &Group) -> Result<Option<Self>, SvgRasterizeError> {
        let transform = self.transform.pre_concat(group.transform());
        if !group.should_isolate() {
            return Ok(Some(Self {
                transform,
                max_bbox: self.max_bbox,
                render_bbox: self.render_bbox,
                pixmap_bounds: self.pixmap_bounds,
            }));
        }
        let inverse = if group.filters().is_empty() {
            Transform::identity()
        } else {
            let sx = transform.sx.hypot(transform.ky);
            let sy = transform.kx.hypot(transform.sy);
            Transform::from_row(
                transform.sx / sx,
                transform.ky / sx,
                transform.kx / sy,
                transform.sy / sy,
                0.0,
                0.0,
            )
            .invert()
            .ok_or(SvgRasterizeError::Allocation)?
        };
        let max_bbox = transform_bbox(self.max_bbox, inverse)?;
        let render_bbox = transform_bbox(self.render_bbox, inverse)?;
        let transform = inverse.pre_concat(transform);
        let bbox = group
            .layer_bounding_box()
            .transform(transform)
            .ok_or(SvgRasterizeError::Allocation)?;
        let ibbox = if group.filters().is_empty() {
            IntRect::from_xywh(
                (bbox.x().floor() as i32)
                    .checked_sub(LAYER_ANTIALIAS_PADDING)
                    .ok_or(SvgRasterizeError::Allocation)?,
                (bbox.y().floor() as i32)
                    .checked_sub(LAYER_ANTIALIAS_PADDING)
                    .ok_or(SvgRasterizeError::Allocation)?,
                (bbox.width().ceil() as u32)
                    .checked_add((LAYER_ANTIALIAS_PADDING * 2) as u32)
                    .ok_or(SvgRasterizeError::Allocation)?,
                (bbox.height().ceil() as u32)
                    .checked_add((LAYER_ANTIALIAS_PADDING * 2) as u32)
                    .ok_or(SvgRasterizeError::Allocation)?,
            )
            .ok_or(SvgRasterizeError::Allocation)?
        } else {
            checked_int_rect(bbox)?
        };
        let Some(ibbox) = ibbox.intersect(&render_bbox) else {
            return Ok(None);
        };
        let dx = bbox.x() - (bbox.x() - ibbox.x() as f32);
        let dy = bbox.y() - (bbox.y() - ibbox.y() as f32);
        Ok(Some(Self {
            transform: Transform::from_translate(-dx, -dy).pre_concat(transform),
            max_bbox,
            render_bbox,
            pixmap_bounds: IntRect::from_xywh(0, 0, ibbox.width(), ibbox.height())
                .ok_or(SvgRasterizeError::Allocation)?,
        }))
    }

    fn for_filter(&self, rect: NonZeroRect) -> Result<Option<Self>, SvgRasterizeError> {
        let Some(region) =
            checked_transformed_int_rect(rect, self.transform)?.intersect(&self.pixmap_bounds)
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            transform: self
                .transform
                .post_translate(-region.x() as f32, -region.y() as f32),
            max_bbox: self.max_bbox,
            render_bbox: self.render_bbox,
            pixmap_bounds: region
                .translate_to(0, 0)
                .ok_or(SvgRasterizeError::Allocation)?,
        }))
    }

    fn for_filter_image(&self, subregion: IntRect) -> Result<Self, SvgRasterizeError> {
        let (sx, sy) = self.transform.get_scale();
        let max_bbox = IntRect::from_xywh(0, 0, self.max_bbox.width(), self.max_bbox.height())
            .ok_or(SvgRasterizeError::Allocation)?;
        Ok(Self {
            transform: Transform::from_row(
                sx,
                0.0,
                0.0,
                sy,
                subregion.x() as f32,
                subregion.y() as f32,
            ),
            max_bbox,
            render_bbox: self.pixmap_bounds,
            pixmap_bounds: self.pixmap_bounds,
        })
    }

    fn pixels(&self) -> u64 {
        u64::from(self.max_bbox.width()) * u64::from(self.max_bbox.height())
    }
}

fn transform_bbox(rect: IntRect, transform: Transform) -> Result<IntRect, SvgRasterizeError> {
    rect.to_rect()
        .transform(transform)
        .and_then(|rect| rect.round_out())
        .ok_or(SvgRasterizeError::Allocation)
}

fn checked_transformed_int_rect(
    rect: NonZeroRect,
    transform: Transform,
) -> Result<IntRect, SvgRasterizeError> {
    checked_int_rect(
        rect.transform(transform)
            .ok_or(SvgRasterizeError::Allocation)?,
    )
}

fn checked_int_rect(rect: NonZeroRect) -> Result<IntRect, SvgRasterizeError> {
    IntRect::from_xywh(
        rect.x().floor() as i32,
        rect.y().floor() as i32,
        (rect.width().ceil() as u32).max(1),
        (rect.height().ceil() as u32).max(1),
    )
    .ok_or(SvgRasterizeError::Allocation)
}

#[derive(Default)]
struct TreeStats {
    nodes: usize,
    max_allocation_pixels: u64,
}

impl TreeStats {
    fn record(&mut self, depth: usize, allocation_pixels: u64) -> Result<(), SvgRasterizeError> {
        self.nodes = self.nodes.saturating_add(1);
        if depth > MAX_TREE_DEPTH || self.nodes > MAX_TREE_NODES {
            return Err(SvgRasterizeError::Complexity);
        }
        if allocation_pixels.saturating_mul(4) > MAX_TOTAL_PIXMAP_BYTES {
            return Err(SvgRasterizeError::Allocation);
        }
        self.max_allocation_pixels = self.max_allocation_pixels.max(allocation_pixels);
        Ok(())
    }
}

fn analyze_tree(
    root: &Group,
    context: Option<&RenderContext>,
) -> Result<TreeStats, SvgRasterizeError> {
    let mut stats = TreeStats::default();
    analyze_group(root, 0, 0, context, &mut stats)?;
    Ok(stats)
}

fn analyze_group(
    group: &Group,
    depth: usize,
    allocation_pixels: u64,
    context: Option<&RenderContext>,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    stats.record(depth, allocation_pixels)?;
    for node in group.children() {
        match node {
            Node::Group(group) => {
                analyze_render_group(group, depth + 1, allocation_pixels, context, stats)?;
            }
            Node::Text(text) => {
                analyze_render_group(
                    text.flattened(),
                    depth + 1,
                    allocation_pixels,
                    context,
                    stats,
                )?;
            }
            Node::Path(path) => {
                stats.record(depth + 1, allocation_pixels)?;
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
    allocation_pixels: u64,
    context: Option<&RenderContext>,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    let transformed_context = context
        .map(|context| context.for_group(group))
        .transpose()?
        .flatten();
    if context.is_some() && transformed_context.is_none() {
        return Ok(());
    }
    let context = transformed_context.as_ref();
    let pixels = context.map_or(0, RenderContext::pixels);
    let layers =
        allocation_pixels.saturating_add(pixels.saturating_mul(u64::from(group.should_isolate())));
    analyze_group(group, depth, layers, context, stats)?;

    if let Some(clip) = group.clip_path() {
        analyze_clip(clip, depth + 1, layers, context, stats)?;
    }
    if let Some(mask) = group.mask() {
        analyze_mask(mask, depth + 1, layers, context, stats)?;
    }
    for filter in group.filters() {
        let filter_context = context
            .map(|context| context.for_filter(filter.rect()))
            .transpose()?
            .flatten();
        if context.is_some() && filter_context.is_none() {
            continue;
        }
        for (retained_results, primitive) in filter.primitives().iter().enumerate() {
            let subregion = filter_context
                .as_ref()
                .map(|context| checked_transformed_int_rect(primitive.rect(), context.transform))
                .transpose()?;
            let retained_results = u64::try_from(retained_results).unwrap_or(u64::MAX);
            let retained_layers = layers.saturating_add(
                pixels.saturating_mul(retained_results.saturating_add(FILTER_SOURCE_BUFFERS)),
            );
            stats.record(
                depth + 1,
                retained_layers.saturating_add(pixels.saturating_mul(FILTER_PRIMITIVE_BUFFERS)),
            )?;
            if let Kind::Image(image) = primitive.kind() {
                let context = filter_context
                    .as_ref()
                    .zip(subregion)
                    .map(|(context, subregion)| context.for_filter_image(subregion))
                    .transpose()?;
                analyze_group(
                    image.root(),
                    depth + 1,
                    retained_layers.saturating_add(pixels),
                    context.as_ref(),
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
    allocation_pixels: u64,
    context: Option<&RenderContext>,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    let layers = allocation_pixels.saturating_add(
        context
            .map_or(0, RenderContext::pixels)
            .saturating_mul(MASK_CLIP_BUFFERS),
    );
    if let Some(context) = context {
        analyze_clip_group(clip.root(), depth, layers, context, stats)?;
    } else {
        analyze_group(clip.root(), depth, layers, None, stats)?;
    }
    if let Some(linked) = clip.clip_path() {
        analyze_clip(linked, depth + 1, layers, context, stats)?;
    }
    Ok(())
}

fn analyze_clip_group(
    group: &Group,
    depth: usize,
    allocation_pixels: u64,
    context: &RenderContext,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    stats.record(depth, allocation_pixels)?;
    for node in group.children() {
        match node {
            Node::Group(group) => {
                let layers = allocation_pixels.saturating_add(
                    context
                        .pixels()
                        .saturating_mul(u64::from(group.clip_path().is_some())),
                );
                analyze_clip_group(group, depth + 1, layers, context, stats)?;
                if let Some(clip) = group.clip_path() {
                    analyze_clip(clip, depth + 1, layers, Some(context), stats)?;
                }
            }
            Node::Text(text) => {
                analyze_clip_group(
                    text.flattened(),
                    depth + 1,
                    allocation_pixels,
                    context,
                    stats,
                )?;
            }
            Node::Path(_) | Node::Image(_) => stats.record(depth + 1, allocation_pixels)?,
        }
    }
    Ok(())
}

fn analyze_mask(
    mask: &Mask,
    depth: usize,
    allocation_pixels: u64,
    context: Option<&RenderContext>,
    stats: &mut TreeStats,
) -> Result<(), SvgRasterizeError> {
    let layers = allocation_pixels.saturating_add(
        context
            .map_or(0, RenderContext::pixels)
            .saturating_mul(MASK_CLIP_BUFFERS),
    );
    analyze_group(mask.root(), depth, layers, context, stats)?;
    if let Some(linked) = mask.mask() {
        analyze_mask(linked, depth + 1, layers, context, stats)?;
    }
    Ok(())
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}
