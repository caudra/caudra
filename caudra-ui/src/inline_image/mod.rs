//! Pictures drawn in the transcript: an image a tool showed or saved, and a
//! local file a message names with `![alt](path)`.
//!
//! Rows are reserved from the image header alone, so nothing jumps when the
//! pixels land. Decoding and resizing run on [`worker`] threads; the UI thread
//! only reads headers and stats files. Everything here lives on the UI thread,
//! which is why the store is thread local rather than threaded through every
//! builder that paints a message or a card.

mod blocks;
mod detect;
mod kitty;
mod worker;

use std::cell::RefCell;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Cursor;
use std::mem;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use caudra_agent::ImageSource;
use caudra_config::InlineImages;
use caudra_terminal_host::mux::TerminalMux;
use image::ImageReader;
use lru::LruCache;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use url::Url;

use crate::image::{MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, media_type_for};
use crate::theme;

use kitty::KittyIds;
use worker::{DecodeJob, DecodeWorker, Payload};

/// Tallest a picture is drawn in the transcript; the viewer shows the rest.
pub(crate) const MAX_IMAGE_ROWS: u16 = 16;
/// Height over width of a cell, for terminals that report no pixel size, as
/// Herdr and tmux often do.
const FALLBACK_CELL_ASPECT: f64 = 2.0;
const FALLBACK_CELL_WIDTH: f64 = 8.0;
/// Enough base64 to reach the dimensions of every format we draw, so a header
/// read does not decode a whole attachment.
const HEADER_PREFIX_CHARS: usize = 64 * 1024;
const DRAWN_CACHE_ENTRIES: usize = 64;
const HEADER_CACHE_ENTRIES: usize = 256;
pub(crate) const LOADING_LABEL: &str = "loading image…";

thread_local! {
    static STORE: RefCell<ImageStore> = RefCell::new(ImageStore::new());
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum ImageMode {
    Kitty,
    /// Quadrant blocks, 2×2 pixels a cell.
    #[default]
    Blocks,
    /// Sextant blocks, 2×3 pixels a cell.
    Sextants,
    Off,
}

/// Pixel size of one terminal cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellSize {
    width: f64,
    height: f64,
}

impl CellSize {
    const FALLBACK: Self = Self {
        width: FALLBACK_CELL_WIDTH,
        height: FALLBACK_CELL_WIDTH * FALLBACK_CELL_ASPECT,
    };

    fn measured() -> Self {
        crossterm::terminal::window_size()
            .ok()
            .filter(|size| size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0)
            .map_or(Self::FALLBACK, |size| Self {
                width: f64::from(size.width) / f64::from(size.columns),
                height: f64::from(size.height) / f64::from(size.rows),
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ImageLayout {
    pub cols: u16,
    pub rows: u16,
}

impl ImageLayout {
    /// The largest size up to the picture's natural one that fits `max_cols`
    /// by `max_rows`, keeping its shape.
    pub(crate) fn fit(
        pixels: (u32, u32),
        cell: CellSize,
        max_cols: u16,
        max_rows: u16,
    ) -> Option<Self> {
        let (width, height) = pixels;
        if width == 0 || height == 0 || max_cols == 0 || max_rows == 0 {
            return None;
        }
        let natural_cols = (f64::from(width) / cell.width).ceil();
        let natural_rows = (f64::from(height) / cell.height).ceil();
        let scale = (f64::from(max_cols) / natural_cols)
            .min(f64::from(max_rows) / natural_rows)
            .min(1.0);
        Some(Self {
            cols: ((natural_cols * scale).round() as u16).clamp(1, max_cols),
            rows: ((natural_rows * scale).round() as u16).clamp(1, max_rows),
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ImageKey {
    Bytes(u64),
    File {
        path: Arc<Path>,
        modified: Option<SystemTime>,
        len: u64,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum ImageOrigin {
    /// Base64, as a tool result or an attachment carries it.
    Bytes(Arc<str>),
    File(Arc<Path>),
}

/// A picture the transcript can draw: where its bytes are, what it is called,
/// and its size in pixels, read from the header.
#[derive(Clone, Debug)]
pub(crate) struct ImageRef {
    key: ImageKey,
    origin: ImageOrigin,
    pixels: (u32, u32),
    alt: Arc<str>,
}

impl ImageRef {
    /// A local file, or `None` when it is missing, not a picture we draw, or
    /// over the size caps.
    pub(crate) fn from_file(path: &Path, alt: &str) -> Option<Self> {
        media_type_for(path)?;
        let meta = fs::metadata(path).ok().filter(|meta| meta.is_file())?;
        if meta.len() > MAX_IMAGE_BYTES as u64 {
            return None;
        }
        let path: Arc<Path> = Arc::from(path);
        let key = ImageKey::File {
            path: Arc::clone(&path),
            modified: meta.modified().ok(),
            len: meta.len(),
        };
        let origin = ImageOrigin::File(path);
        let pixels = with_store(|store| store.pixels(&key, &origin))?;
        Some(Self {
            key,
            origin,
            pixels,
            alt: Arc::from(alt),
        })
    }

    pub(crate) fn from_source(source: &ImageSource, alt: &str) -> Option<Self> {
        let mut hasher = DefaultHasher::new();
        source.data.hash(&mut hasher);
        let key = ImageKey::Bytes(hasher.finish());
        let origin = ImageOrigin::Bytes(Arc::clone(&source.data));
        let pixels = with_store(|store| store.pixels(&key, &origin))?;
        Some(Self {
            key,
            origin,
            pixels,
            alt: Arc::from(alt),
        })
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        match &self.origin {
            ImageOrigin::File(path) => Some(path),
            ImageOrigin::Bytes(_) => None,
        }
    }

    pub(crate) fn alt(&self) -> &str {
        &self.alt
    }

    pub(crate) fn pixels(&self) -> (u32, u32) {
        self.pixels
    }

    /// What copying the picture gives: markdown that names it again.
    pub(crate) fn markdown(&self) -> String {
        let target = self
            .path()
            .map_or_else(String::new, |path| path.display().to_string());
        format!("![{}]({target})", self.alt)
    }
}

/// Rows a picture occupies in a segment's lines.
#[derive(Clone, Debug)]
pub(crate) struct ImageSpan {
    pub rows: Range<usize>,
    pub image: ImageRef,
}

/// The style every cell of a drawn picture carries. Pinning reverse video off
/// keeps a highlighted card from inverting the pixels, and it is how
/// [`is_pixel`] tells a picture's cells from text.
pub(crate) fn pixel_style() -> Style {
    Style::new().remove_modifier(Modifier::REVERSED)
}

/// Whether a span is part of a picture, which a hover tint must leave alone.
pub(crate) fn is_pixel(style: Style) -> bool {
    style.sub_modifier.contains(Modifier::REVERSED)
}

/// Lines for `image` at most `max_cols` wide, or `None` when pictures are off
/// or this one cannot be drawn, in which case the caller keeps its label.
pub(crate) fn draw(image: &ImageRef, max_cols: u16) -> Option<Vec<Line<'static>>> {
    draw_fitted(image, max_cols, MAX_IMAGE_ROWS)
}

/// [`draw`] into a box of `max_cols` by `max_rows`, for a view with room to
/// spare, such as the viewer or the workbench.
pub(crate) fn draw_fitted(
    image: &ImageRef,
    max_cols: u16,
    max_rows: u16,
) -> Option<Vec<Line<'static>>> {
    with_store(|store| store.draw(image, max_cols, max_rows))
}

/// Whether a decode is in flight, so the loop keeps ticking until it lands.
pub(crate) fn is_pending() -> bool {
    with_store(|store| !store.pending.is_empty())
}

/// Resolves the target of a markdown image to a local picture. Only local
/// files are drawn: a URL with any scheme but `file:` is never fetched, and a
/// session whose workspace is remote never reads this machine's disk.
pub(crate) fn resolve_markdown(src: &str, alt: &str) -> Option<ImageRef> {
    let root = with_store(|store| (store.mode != ImageMode::Off).then(|| store.root.clone()))??;
    ImageRef::from_file(&local_path(src, &root)?, alt)
}

fn local_path(src: &str, root: &Path) -> Option<PathBuf> {
    let src = src.trim();
    // A drive letter parses as a one-letter scheme.
    if let Ok(url) = Url::parse(src)
        && url.scheme().len() > 1
    {
        return (url.scheme() == "file")
            .then(|| url.to_file_path().ok())
            .flatten();
    }
    let path = Path::new(src);
    Some(if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    })
}

/// Bumps whenever a drawing could change: a decode landed, or the mode, the
/// cell size or the workspace moved. A panel that saw an older value rebuilds
/// its pictures.
pub(crate) fn generation() -> u64 {
    with_store(|store| {
        store.drain();
        store.generation
    })
}

/// Settles how pictures are drawn for this terminal. Has to run in raw mode
/// and before the input reader starts, for the reason [`detect::detect`] gives.
pub(crate) fn configure(setting: InlineImages) {
    let mode = detect::detect(setting);
    with_store(|store| store.mux = TerminalMux::detect());
    set_mode(mode);
    refresh_cell_size();
}

pub(crate) fn set_mode(mode: ImageMode) {
    with_store(|store| {
        if store.mode != mode {
            store.mode = mode;
            store.generation += 1;
        }
    });
}

/// Escapes owed to the terminal before the next frame: Kitty uploads and the
/// deletes that free recycled ids. Written outside the frame so the drawing
/// never carries image data.
pub(crate) fn take_output() -> String {
    with_store(|store| {
        store.drain();
        mem::take(&mut store.output)
    })
}

/// Drops every picture the terminal holds, for when the terminal may have
/// lost them, such as after handing it to another program. They are uploaded
/// again as they are drawn.
pub(crate) fn forget_uploads() {
    with_store(|store| {
        if store.mode != ImageMode::Kitty {
            return;
        }
        store.kitty.clear();
        store.drawn.clear();
        store.generation += 1;
    });
}

/// What frees every uploaded picture on the way out, or nothing when none
/// could have been uploaded.
pub(crate) fn exit_output() -> Option<String> {
    with_store(|store| {
        (store.mode == ImageMode::Kitty).then(|| store.mux.wrap(kitty::delete_all()))
    })
}

/// The directory a relative markdown image resolves against, or `None` for a
/// remote workspace, whose paths mean nothing on this machine.
pub(crate) fn set_local_root(root: Option<&Path>) {
    with_store(|store| {
        if store.root.as_deref() != root {
            store.root = root.map(Arc::from);
            store.generation += 1;
        }
    });
}

/// Re-reads the cell pixel size, which changes with the font and with a
/// resize that reports one.
pub(crate) fn refresh_cell_size() {
    let cell = CellSize::measured();
    with_store(|store| {
        if store.cell != cell {
            store.cell = cell;
            store.generation += 1;
        }
    });
}

fn with_store<T>(f: impl FnOnce(&mut ImageStore) -> T) -> T {
    STORE.with_borrow_mut(f)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DrawKey {
    image: ImageKey,
    layout: ImageLayout,
    mode: ImageMode,
}

enum Drawn {
    Lines(Arc<[Line<'static>]>),
    Kitty(u8),
    Failed,
}

struct ImageStore {
    mode: ImageMode,
    cell: CellSize,
    root: Option<Arc<Path>>,
    generation: u64,
    worker: DecodeWorker,
    headers: LruCache<ImageKey, Option<(u32, u32)>>,
    drawn: LruCache<DrawKey, Drawn>,
    pending: Vec<DrawKey>,
    kitty: KittyIds,
    mux: TerminalMux,
    output: String,
}

impl ImageStore {
    fn new() -> Self {
        Self {
            mode: ImageMode::default(),
            cell: CellSize::FALLBACK,
            root: None,
            generation: 0,
            worker: DecodeWorker::new(),
            headers: LruCache::new(NonZeroUsize::new(HEADER_CACHE_ENTRIES).unwrap()),
            drawn: LruCache::new(NonZeroUsize::new(DRAWN_CACHE_ENTRIES).unwrap()),
            pending: Vec::new(),
            kitty: KittyIds::new(),
            mux: TerminalMux::None,
            output: String::new(),
        }
    }

    fn pixels(&mut self, key: &ImageKey, origin: &ImageOrigin) -> Option<(u32, u32)> {
        if let Some(pixels) = self.headers.get(key) {
            return *pixels;
        }
        let pixels = read_dimensions(origin).filter(|&(width, height)| {
            (width as usize).saturating_mul(height as usize) <= MAX_IMAGE_PIXELS
        });
        self.headers.put(key.clone(), pixels);
        pixels
    }

    fn draw(
        &mut self,
        image: &ImageRef,
        max_cols: u16,
        max_rows: u16,
    ) -> Option<Vec<Line<'static>>> {
        if self.mode == ImageMode::Off {
            return None;
        }
        let (max_cols, max_rows) = match self.mode {
            ImageMode::Kitty => (
                max_cols.min(kitty::MAX_CELLS),
                max_rows.min(kitty::MAX_CELLS),
            ),
            ImageMode::Blocks | ImageMode::Sextants | ImageMode::Off => (max_cols, max_rows),
        };
        let layout = ImageLayout::fit(image.pixels, self.cell, max_cols, max_rows)?;
        let key = DrawKey {
            image: image.key.clone(),
            layout,
            mode: self.mode,
        };
        match self.drawn.get(&key) {
            Some(Drawn::Lines(lines)) => {
                return Some(lines.to_vec());
            }
            Some(&Drawn::Kitty(id)) => {
                self.kitty.touch(id);
                return Some(kitty::placeholder_lines(id, layout));
            }
            Some(Drawn::Failed) => return None,
            None => {}
        }
        if !self.pending.contains(&key) {
            self.worker.send(DecodeJob {
                key: key.clone(),
                origin: image.origin.clone(),
                layout,
                cell: self.cell,
            });
            self.pending.push(key);
        }
        Some(loading_lines(layout, max_cols))
    }

    fn drain(&mut self) {
        while let Some(decoded) = self.worker.try_recv() {
            self.pending.retain(|key| *key != decoded.key);
            let drawn = match decoded.result {
                Ok(Payload::Lines(lines)) => Drawn::Lines(lines.into()),
                Ok(Payload::Png(png)) => Drawn::Kitty(self.upload(&decoded.key, &png)),
                Err(error) => {
                    tracing::warn!(%error, "inline image did not decode");
                    Drawn::Failed
                }
            };
            self.drawn.put(decoded.key, drawn);
            self.generation += 1;
        }
    }

    /// Queues `png` for the terminal under a fresh id, deleting whatever
    /// picture held the id last.
    fn upload(&mut self, key: &DrawKey, png: &[u8]) -> u8 {
        let (id, evicted) = self.kitty.assign(key.clone());
        if let Some(evicted) = evicted {
            self.drawn.pop(&evicted);
            self.output.push_str(&self.mux.wrap(kitty::delete(id)));
        }
        for escape in kitty::transmit(id, png, key.layout) {
            self.output.push_str(&self.mux.wrap(escape));
        }
        id
    }
}

fn loading_lines(layout: ImageLayout, max_cols: u16) -> Vec<Line<'static>> {
    let label: String = LOADING_LABEL.chars().take(max_cols.into()).collect();
    let mut lines = vec![Line::default(); layout.rows.into()];
    lines[0] = Line::from(Span::styled(label, theme::current().tool_dim));
    lines
}

fn read_dimensions(origin: &ImageOrigin) -> Option<(u32, u32)> {
    match origin {
        ImageOrigin::File(path) => ImageReader::open(path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok(),
        ImageOrigin::Bytes(data) => {
            let prefix = &data[..data.len().min(HEADER_PREFIX_CHARS) / 4 * 4];
            dimensions_of(&STANDARD.decode(prefix).ok()?)
                .or_else(|| dimensions_of(&STANDARD.decode(data.as_bytes()).ok()?))
        }
    }
}

fn dimensions_of(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::sync::Arc;

    use super::{
        CellSize, ImageLayout, ImageMode, ImageRef, ImageSource, LOADING_LABEL, MAX_IMAGE_ROWS,
        STANDARD, blocks, draw, generation, is_pending, is_pixel, resolve_markdown, set_local_root,
        set_mode, with_store,
    };
    use base64::Engine;
    use image::{ImageFormat, Rgba, RgbaImage};
    use tempfile::TempDir;
    use test_case::test_case;
    use url::Url;

    /// The symbol every opaque cell of a picture drawn as blocks carries.
    pub(crate) const PIXEL_SYMBOL: &str = blocks::UPPER_HALF;
    const ALT: &str = "A plot";
    const PICTURE: &str = "plot.png";
    const NOT_A_PICTURE: &str = "notes.txt";

    pub(crate) fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([200, 10, 10, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    pub(crate) fn png_source(width: u32, height: u32) -> ImageSource {
        ImageSource::new(
            caudra_agent::ImageMediaType::Png,
            Arc::from(STANDARD.encode(png_bytes(width, height))),
        )
    }

    /// Runs every queued decode on this thread and takes in the results, so a
    /// test sees the pixels without waiting on a worker.
    pub(crate) fn decode_queued() {
        with_store(|store| {
            store.worker.run_queued();
            store.drain();
        });
    }

    fn workspace_with_picture() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(PICTURE), png_bytes(16, 32)).unwrap();
        fs::write(dir.path().join(NOT_A_PICTURE), "text").unwrap();
        dir
    }

    #[test_case((80, 160), 40, (10, 10) ; "natural_size_fits")]
    #[test_case((800, 160), 40, (40, 4) ; "wide_scales_to_columns")]
    #[test_case((80, 1600), 40, (2, 16) ; "tall_scales_to_row_cap")]
    #[test_case((1, 1), 40, (1, 1) ; "tiny_keeps_one_cell")]
    fn layout_fits_the_box(pixels: (u32, u32), max_cols: u16, expected: (u16, u16)) {
        let layout =
            ImageLayout::fit(pixels, CellSize::FALLBACK, max_cols, MAX_IMAGE_ROWS).unwrap();
        assert_eq!((layout.cols, layout.rows), expected);
    }

    #[test_case("plot.png", true ; "relative_to_workspace")]
    #[test_case("./plot.png", true ; "dotted_relative")]
    #[test_case("notes.txt", false ; "not_an_image_extension")]
    #[test_case("missing.png", false ; "missing_file")]
    #[test_case("http://example.com/plot.png", false ; "remote_url_never_fetched")]
    #[test_case("https://example.com/plot.png", false ; "remote_https_never_fetched")]
    fn markdown_targets_resolve_to_local_files_only(src: &str, resolves: bool) {
        let dir = workspace_with_picture();
        set_mode(ImageMode::Blocks);
        set_local_root(Some(dir.path()));
        let image = resolve_markdown(src, ALT);
        assert_eq!(image.is_some(), resolves);
        if let Some(image) = image {
            assert_eq!(image.pixels(), (16, 32));
            assert_eq!(image.path(), Some(dir.path().join(PICTURE).as_path()));
        }
    }

    #[test]
    fn a_file_url_resolves_to_its_path() {
        let dir = workspace_with_picture();
        set_mode(ImageMode::Blocks);
        set_local_root(Some(dir.path()));
        let url = Url::from_file_path(dir.path().join(PICTURE)).unwrap();
        assert!(resolve_markdown(url.as_str(), ALT).is_some());
    }

    #[test]
    fn a_remote_workspace_never_reads_local_paths() {
        let dir = workspace_with_picture();
        set_mode(ImageMode::Blocks);
        set_local_root(None);
        let absolute = dir.path().join(PICTURE);
        assert!(resolve_markdown(absolute.to_str().unwrap(), ALT).is_none());
        assert!(resolve_markdown(PICTURE, ALT).is_none());
    }

    #[test]
    fn pictures_off_resolve_nothing() {
        let dir = workspace_with_picture();
        set_local_root(Some(dir.path()));
        set_mode(ImageMode::Off);
        assert!(resolve_markdown(PICTURE, ALT).is_none());
        set_mode(ImageMode::Blocks);
    }

    #[test]
    fn bytes_read_their_size_from_the_header() {
        let image = ImageRef::from_source(&png_source(24, 8), ALT).unwrap();
        assert_eq!(image.pixels(), (24, 8));
        assert_eq!(image.markdown(), format!("![{ALT}]()"));
    }

    #[test]
    fn rows_are_reserved_while_decoding_and_filled_after() {
        set_mode(ImageMode::Blocks);
        let image = ImageRef::from_source(&png_source(16, 64), ALT).unwrap();
        let before = generation();
        let loading = draw(&image, 40).unwrap();
        assert!(is_pending());
        assert_eq!(loading.len(), 4);
        assert_eq!(loading[0].to_string(), LOADING_LABEL);

        decode_queued();
        assert!(generation() > before);
        let drawn = draw(&image, 40).unwrap();
        assert!(!is_pending());
        assert_eq!(drawn.len(), loading.len());
        assert!(
            drawn
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| is_pixel(span.style))
        );
    }

    #[test]
    fn copying_a_file_picture_names_its_path() {
        let dir = workspace_with_picture();
        let path = dir.path().join(PICTURE);
        let image = ImageRef::from_file(&path, ALT).unwrap();
        assert_eq!(image.markdown(), format!("![{ALT}]({})", path.display()));
    }
}
