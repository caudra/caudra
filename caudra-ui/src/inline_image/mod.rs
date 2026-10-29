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

#[cfg(test)]
mod compatibility;

use std::cell::RefCell;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Cursor;
use std::mem;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use caudra_agent::ImageSource;
use caudra_config::InlineImages;
use caudra_storage::StateDir;
use caudra_storage::media::MediaStore;
use caudra_terminal_host::mux::TerminalMux;
use caudra_workbench::{ImageSampling, ImageViewport, PaintedImage};
use image::ImageReader;
use lru::LruCache;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use url::Url;

use crate::image::{MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, media_type_for};
use crate::theme;

use kitty::KittyIds;
use worker::{DecodeJob, DecodeWorker, Payload, Ticket};

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
const MAX_RASTER_PIXELS: u32 = 1_000_000;
const MAX_RASTER_EDGE: u32 = 4096;
const DRAWN_CACHE_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const LOADING_LABEL: &str = "loading image…";

static MEDIA: LazyLock<Option<MediaStore>> =
    LazyLock::new(|| StateDir::resolve().ok().map(|dir| MediaStore::new(&dir)));

pub(crate) fn media_store() -> Option<&'static MediaStore> {
    MEDIA.as_ref()
}

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
    source: &'static str,
}

impl CellSize {
    const FALLBACK: Self = Self {
        width: FALLBACK_CELL_WIDTH,
        height: FALLBACK_CELL_WIDTH * FALLBACK_CELL_ASPECT,
        source: "estimated",
    };

    fn measured(reported: Option<(u16, u16)>) -> Self {
        crossterm::terminal::window_size()
            .ok()
            .filter(|size| size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0)
            .map_or_else(
                || {
                    reported
                        .filter(|&(w, h)| w > 0 && h > 0)
                        .map_or(Self::FALLBACK, |(w, h)| Self {
                            width: w.into(),
                            height: h.into(),
                            source: "reported",
                        })
                },
                |size| Self {
                    width: f64::from(size.width) / f64::from(size.columns),
                    height: f64::from(size.height) / f64::from(size.rows),
                    source: "measured",
                },
            )
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

impl ImageKey {
    fn current(&self) -> bool {
        match self {
            Self::Bytes(_) => true,
            Self::File {
                path,
                modified,
                len,
            } => fs::metadata(path)
                .is_ok_and(|meta| meta.len() == *len && meta.modified().ok() == *modified),
        }
    }
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
        Self::from_picture_file(path, alt)
    }

    fn from_picture_file(path: &Path, alt: &str) -> Option<Self> {
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

    /// The image's bytes, or for a reference whose bytes are not loaded, its
    /// file in the media store. `None` when that file is gone.
    pub(crate) fn from_source(source: &ImageSource, alt: &str) -> Option<Self> {
        let Some(data) = source.data() else {
            let path = media_store()?.path(&source.stored()?.sha256);
            return Self::from_picture_file(&path, alt);
        };
        if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let key = ImageKey::Bytes(hasher.finish());
        let origin = ImageOrigin::Bytes(Arc::clone(data));
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

pub(crate) fn draw_viewport(
    image: &ImageRef,
    viewport: &mut ImageViewport,
    area: Rect,
) -> Option<PaintedImage> {
    with_store(|store| {
        if store.mode == ImageMode::Off {
            viewport.clear_geometry();
            return None;
        }
        let area = Rect {
            width: area.width.min(kitty::MAX_CELLS),
            height: area.height.min(kitty::MAX_CELLS),
            ..area
        };
        let cell = match store.mode {
            ImageMode::Kitty => (store.cell.width, store.cell.height),
            ImageMode::Blocks | ImageMode::Sextants | ImageMode::Off => {
                let rows = if store.mode == ImageMode::Sextants {
                    3.0
                } else {
                    2.0
                };
                let samples = (2.0 / store.cell.width).min(rows / store.cell.height);
                (store.cell.width * samples, store.cell.height * samples)
            }
        };
        let geometry = viewport.place(image.pixels, cell, area)?;
        let layout = ImageLayout {
            cols: geometry.area.width,
            rows: geometry.area.height,
        };
        let mut key = store.key(image, layout, geometry.crop, viewport.sampling);
        key.set_content(geometry.content, geometry.offset);
        let Some(lines) = store.request(image, key, Some(viewport.lifetime())) else {
            viewport.clear_geometry();
            return None;
        };
        let renderer = match store.mode {
            ImageMode::Kitty => store.cell.source,
            ImageMode::Blocks => "blocks · samples",
            ImageMode::Sextants => "sextants · samples",
            ImageMode::Off => "off",
        };
        let renderer = if store.mode == ImageMode::Kitty {
            format!("Kitty · {renderer} pixels")
        } else {
            renderer.to_owned()
        };
        let status = format!(
            "{:.0}% · {renderer} · {}",
            geometry.scale * 100.0,
            viewport.sampling.label()
        );
        Some(PaintedImage {
            lines,
            geometry,
            status,
        })
    })
}

/// Whether a decode is in flight, so the loop keeps ticking until it lands.
pub(crate) fn is_pending() -> bool {
    with_store(|store| store.pending.iter().any(|(_, ticket)| ticket.current()))
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
    let detected = detect::detect(setting);
    with_store(|store| {
        store.mux = TerminalMux::detect();
        store.reported_cell = detected.cell_pixels;
    });
    set_mode(detected.mode);
    refresh_cell_size();
}

pub(crate) fn set_mode(mode: ImageMode) {
    with_store(|store| {
        if store.mode != mode {
            store.reset();
            store.mode = mode;
            store.generation += 1;
        }
    });
}

/// Escapes owed to the terminal before the next frame: Kitty uploads and the
/// deletes that free recycled ids. Written outside the frame so the drawing
/// never carries image data.
pub(crate) fn take_output() -> String {
    with_store(ImageStore::output)
}

/// Drops every picture the terminal holds, for when the terminal may have
/// lost them, such as after handing it to another program. They are uploaded
/// again as they are drawn.
pub(crate) fn forget_uploads() {
    with_store(|store| {
        store.reset();
        store.generation += 1;
    });
}

/// What frees every uploaded picture on the way out, or nothing when none
/// could have been uploaded.
pub(crate) fn exit_output() -> Option<String> {
    with_store(|store| {
        store.reset();
        (!store.output.is_empty()).then(|| mem::take(&mut store.output))
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
    with_store(|store| {
        let cell = CellSize::measured(store.reported_cell);
        if store.cell != cell {
            store.reset();
            store.cell = cell;
            store.generation += 1;
            tracing::info!(width = cell.width, height = cell.height, source = cell.source, mode = ?store.mode, "inline image cell size");
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
    crop: (u32, u32, u32, u32),
    target: (u32, u32),
    content: (u32, u32),
    offset: (i32, i32),
    sampling: ImageSampling,
}

impl DrawKey {
    fn set_content(&mut self, content: (f64, f64), offset: (f64, f64)) {
        let pixels_per_cell = (
            f64::from(self.target.0) / f64::from(self.layout.cols),
            f64::from(self.target.1) / f64::from(self.layout.rows),
        );
        let content = (
            (content.0 * pixels_per_cell.0).round().max(1.0),
            (content.1 * pixels_per_cell.1).round().max(1.0),
        );
        let bound = (f64::from(MAX_RASTER_PIXELS) / (content.0 * content.1))
            .sqrt()
            .min(f64::from(MAX_RASTER_EDGE) / content.0.max(content.1))
            .min(1.0);
        self.content = (
            ((content.0 * bound).floor() as u32).max(1),
            ((content.1 * bound).floor() as u32).max(1),
        );
        self.offset = (
            (offset.0 * pixels_per_cell.0 * bound).round() as i32,
            (offset.1 * pixels_per_cell.1 * bound).round() as i32,
        );
        self.target = (
            ((f64::from(self.target.0) * bound).floor() as u32).max(1),
            ((f64::from(self.target.1) * bound).floor() as u32).max(1),
        );
    }
}

enum Drawn {
    Lines(Arc<[Line<'static>]>),
    Kitty(u8),
    Failed,
}

impl Drawn {
    fn bytes(&self) -> usize {
        match self {
            Self::Lines(lines) => lines
                .iter()
                .map(|line| {
                    mem::size_of::<Line<'_>>()
                        + line
                            .spans
                            .iter()
                            .map(|span| mem::size_of::<Span<'_>>() + span.content.len())
                            .sum::<usize>()
                })
                .sum(),
            Self::Kitty(_) | Self::Failed => mem::size_of::<Self>(),
        }
    }
}

struct Upload {
    key: DrawKey,
    id: u8,
    png: Vec<u8>,
    ticket: Ticket,
}

struct ImageStore {
    mode: ImageMode,
    cell: CellSize,
    reported_cell: Option<(u16, u16)>,
    root: Option<Arc<Path>>,
    generation: u64,
    worker: DecodeWorker,
    headers: LruCache<ImageKey, Option<(u32, u32)>>,
    drawn: LruCache<DrawKey, Drawn>,
    pending: Vec<(DrawKey, Ticket)>,
    kitty: KittyIds,
    mux: TerminalMux,
    output: String,
    uploads: Vec<Upload>,
}

impl ImageStore {
    fn reset(&mut self) {
        for (_, ticket) in self.pending.drain(..) {
            ticket.cancel();
        }
        self.uploads.clear();
        for id in self.kitty.ids() {
            self.output.push_str(&self.mux.wrap(kitty::delete(id)));
        }
        self.kitty.clear();
        self.drawn.clear();
    }

    fn new() -> Self {
        Self {
            mode: ImageMode::default(),
            cell: CellSize::FALLBACK,
            reported_cell: None,
            root: None,
            generation: 0,
            worker: DecodeWorker::new(),
            headers: LruCache::new(NonZeroUsize::new(HEADER_CACHE_ENTRIES).unwrap()),
            drawn: LruCache::new(NonZeroUsize::new(DRAWN_CACHE_ENTRIES).unwrap()),
            pending: Vec::new(),
            kitty: KittyIds::new(),
            mux: TerminalMux::None,
            output: String::new(),
            uploads: Vec::new(),
        }
    }

    fn pixels(&mut self, key: &ImageKey, origin: &ImageOrigin) -> Option<(u32, u32)> {
        if let Some(pixels) = self.headers.get(key) {
            return *pixels;
        }
        let pixels = read_dimensions(origin).filter(|&(width, height)| {
            width > 0
                && height > 0
                && (width as usize).saturating_mul(height as usize) <= MAX_IMAGE_PIXELS
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
        let (max_cols, max_rows) = (
            max_cols.min(kitty::MAX_CELLS),
            max_rows.min(kitty::MAX_CELLS),
        );
        let layout = ImageLayout::fit(image.pixels, self.cell, max_cols, max_rows)?;
        let key = self.key(
            image,
            layout,
            (0, 0, image.pixels.0, image.pixels.1),
            ImageSampling::Smooth,
        );
        self.request(image, key, None)
    }

    fn key(
        &self,
        image: &ImageRef,
        layout: ImageLayout,
        crop: (u32, u32, u32, u32),
        sampling: ImageSampling,
    ) -> DrawKey {
        let (width, height) = match self.mode {
            ImageMode::Kitty => (
                (f64::from(layout.cols) * self.cell.width).ceil() as u32,
                (f64::from(layout.rows) * self.cell.height).ceil() as u32,
            ),
            ImageMode::Sextants => (u32::from(layout.cols) * 2, u32::from(layout.rows) * 3),
            ImageMode::Blocks | ImageMode::Off => {
                (u32::from(layout.cols) * 2, u32::from(layout.rows) * 2)
            }
        };
        let scale = (f64::from(MAX_RASTER_PIXELS) / (f64::from(width) * f64::from(height)))
            .sqrt()
            .min(f64::from(MAX_RASTER_EDGE) / f64::from(width.max(height)))
            .min(1.0);
        let mut key = DrawKey {
            image: image.key.clone(),
            layout,
            mode: self.mode,
            crop,
            target: (
                ((f64::from(width) * scale) as u32).max(1),
                ((f64::from(height) * scale) as u32).max(1),
            ),
            content: (1, 1),
            offset: (0, 0),
            sampling,
        };
        let source_scale = (f64::from(layout.cols) * self.cell.width / f64::from(crop.2))
            .min(f64::from(layout.rows) * self.cell.height / f64::from(crop.3))
            .min(1.0);
        let content = (
            f64::from(crop.2) * source_scale / self.cell.width,
            f64::from(crop.3) * source_scale / self.cell.height,
        );
        key.set_content(
            content,
            (
                (f64::from(layout.cols) - content.0) / 2.0,
                (f64::from(layout.rows) - content.1) / 2.0,
            ),
        );
        key
    }

    fn request(
        &mut self,
        image: &ImageRef,
        key: DrawKey,
        owner: Option<Weak<()>>,
    ) -> Option<Vec<Line<'static>>> {
        if !image.key.current() {
            return None;
        }
        let layout = key.layout;
        self.pending.retain(|(pending, ticket)| {
            if owner
                .as_ref()
                .zip(ticket.owner.as_ref())
                .is_some_and(|(a, b)| a.ptr_eq(b))
                && *pending != key
            {
                ticket.cancel();
            }
            ticket.current()
        });
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
        if self.mode == ImageMode::Kitty
            && let Some(id) = self.kitty.find(&key)
        {
            self.drawn.put(key, Drawn::Kitty(id));
            return Some(kitty::placeholder_lines(id, layout));
        }
        if !self.pending.iter().any(|(pending, ticket)| {
            *pending == key
                && match (&owner, &ticket.owner) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.ptr_eq(b),
                    _ => false,
                }
        }) && self.pending.len() < worker::MAX_JOBS
        {
            let ticket = Ticket::new(owner);
            if self.worker.send(DecodeJob {
                key: key.clone(),
                origin: image.origin.clone(),
                pixels: image.pixels,
                ticket: ticket.clone(),
            }) {
                self.pending.push((key, ticket));
            }
        }
        Some(loading_lines(layout))
    }

    fn drain(&mut self) {
        self.pending.retain(|(_, ticket)| ticket.current());
        while let Some(decoded) = self.worker.try_recv() {
            if !decoded.ticket.current() || decoded.key.mode != self.mode {
                continue;
            }
            self.pending.retain(|(key, _)| *key != decoded.key);
            if !decoded.key.image.current() {
                self.generation += 1;
                continue;
            }
            if self.drawn.contains(&decoded.key) {
                continue;
            }
            let drawn = match decoded.result {
                Ok(Payload::Lines(lines)) => Drawn::Lines(lines.into()),
                Ok(Payload::Png(png)) => {
                    Drawn::Kitty(self.upload(&decoded.key, png, decoded.ticket))
                }
                Err(error) => {
                    tracing::warn!(%error, "inline image did not decode");
                    Drawn::Failed
                }
            };
            while self
                .drawn
                .iter()
                .map(|(_, entry)| entry.bytes())
                .sum::<usize>()
                + drawn.bytes()
                > DRAWN_CACHE_BYTES
            {
                if self.drawn.pop_lru().is_none() {
                    break;
                }
            }
            self.drawn.put(decoded.key, drawn);
            self.generation += 1;
        }
    }

    /// Queues `png` for the terminal under a fresh id, deleting whatever
    /// picture held the id last.
    fn upload(&mut self, key: &DrawKey, png: Vec<u8>, ticket: Ticket) -> u8 {
        for (id, evicted) in self.kitty.make_room(key) {
            self.drawn.pop(&evicted);
            self.uploads.retain(|upload| upload.id != id);
            self.output.push_str(&self.mux.wrap(kitty::delete(id)));
        }
        let (id, evicted) = self.kitty.assign(key.clone());
        if let Some(evicted) = evicted {
            self.drawn.pop(&evicted);
            self.uploads.retain(|upload| upload.id != id);
            self.output.push_str(&self.mux.wrap(kitty::delete(id)));
        }
        self.uploads.push(Upload {
            key: key.clone(),
            id,
            png,
            ticket,
        });
        id
    }

    fn output(&mut self) -> String {
        for upload in mem::take(&mut self.uploads) {
            if !upload.ticket.current() || !upload.key.image.current() {
                self.drawn.pop(&upload.key);
                self.kitty.remove(upload.id);
                self.output
                    .push_str(&self.mux.wrap(kitty::delete(upload.id)));
                self.generation += 1;
                continue;
            }
            for escape in kitty::transmit(upload.id, &upload.png, upload.key.layout) {
                self.output.push_str(&self.mux.wrap(escape));
            }
        }
        mem::take(&mut self.output)
    }
}

fn loading_lines(layout: ImageLayout) -> Vec<Line<'static>> {
    let label: String = LOADING_LABEL.chars().take(layout.cols.into()).collect();
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
        CellSize, Drawn, ImageLayout, ImageMode, ImageRef, ImageSource, ImageStore, LOADING_LABEL,
        MAX_IMAGE_ROWS, STANDARD, Ticket, blocks, draw, draw_viewport, forget_uploads, generation,
        is_pending, is_pixel, resolve_markdown, set_local_root, set_mode, take_output, with_store,
    };
    use base64::Engine;
    use caudra_workbench::{ImageSampling, ImageViewport, keys};
    use image::{ImageFormat, Rgba, RgbaImage};
    use ratatui::layout::Rect;
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
        assert_eq!(
            loading[0].to_string(),
            LOADING_LABEL.chars().take(2).collect::<String>()
        );

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

    #[test_case(1; "single_column")]
    #[test_case(2; "narrow")]
    #[test_case(40; "wide")]
    fn loading_rows_never_exceed_requested_width(cols: u16) {
        let image = ImageRef::from_source(&png_source(320, 160), ALT).unwrap();
        let mut store = ImageStore::new();
        let lines = store.draw(&image, cols, MAX_IMAGE_ROWS).unwrap();
        assert!(lines.iter().all(|line| line.width() <= usize::from(cols)));
        assert!(!store.pending.is_empty());
    }

    #[test]
    fn identical_cells_with_different_pixel_sizes_miss_the_cache() {
        let image = ImageRef::from_source(&png_source(80, 80), ALT).unwrap();
        let mut store = ImageStore::new();
        store.mode = ImageMode::Kitty;
        let layout = ImageLayout { cols: 4, rows: 2 };
        let first = store.key(&image, layout, (0, 0, 80, 80), ImageSampling::Smooth);
        store.cell.width *= 2.0;
        store.cell.height *= 2.0;
        let second = store.key(&image, layout, first.crop, ImageSampling::Smooth);
        assert_ne!(first, second);
        assert_eq!(first.layout, second.layout);
        assert_eq!(second.target, (first.target.0 * 2, first.target.1 * 2));
    }

    #[test_case((100, 100), (104, 112), (100, 100), (2, 6); "partial_cells")]
    #[test_case((9, 9), (16, 16), (9, 9), (4, 4); "small_image")]
    fn viewport_keys_keep_actual_content_separate_from_cell_padding(
        source: (u32, u32),
        target: (u32, u32),
        content: (u32, u32),
        offset: (i32, i32),
    ) {
        let image = ImageRef::from_source(&png_source(source.0, source.1), ALT).unwrap();
        let mut store = ImageStore::new();
        store.mode = ImageMode::Kitty;
        let mut viewport = ImageViewport::default();
        viewport.handle_key(keys::IMAGE_ACTUAL.to_key_event());
        let g = viewport
            .place(
                source,
                (store.cell.width, store.cell.height),
                Rect::new(0, 0, 80, 40),
            )
            .unwrap();
        let mut key = store.key(
            &image,
            ImageLayout {
                cols: g.area.width,
                rows: g.area.height,
            },
            g.crop,
            ImageSampling::Nearest,
        );
        key.set_content(g.content, g.offset);
        assert_eq!(key.target, target);
        assert_eq!(key.content, content);
        assert_eq!(key.offset, offset);
        let mut moved = key.clone();
        moved.offset.0 += 1;
        assert_ne!(key, moved);
    }

    #[test]
    fn cropped_raster_and_canvas_share_allocation_bounds() {
        let image = ImageRef::from_source(&png_source(100, 100), ALT).unwrap();
        let mut store = ImageStore::new();
        store.mode = ImageMode::Kitty;
        let mut key = store.key(
            &image,
            ImageLayout {
                cols: 297,
                rows: 297,
            },
            (0, 0, 100, 100),
            ImageSampling::Nearest,
        );
        key.set_content((400.0, 400.0), (-10.0, -10.0));
        for dimensions in [key.target, key.content] {
            assert!(
                u64::from(dimensions.0) * u64::from(dimensions.1)
                    <= u64::from(super::MAX_RASTER_PIXELS)
            );
            assert!(dimensions.0.max(dimensions.1) <= super::MAX_RASTER_EDGE);
        }
    }

    #[test]
    fn drawing_never_drains_a_completed_worker() {
        let image = ImageRef::from_source(&png_source(80, 80), ALT).unwrap();
        let mut store = ImageStore::new();
        let loading = store.draw(&image, 40, 16).unwrap();
        store.worker.run_queued();
        let before = store.generation;
        assert_eq!(store.draw(&image, 40, 16).unwrap(), loading);
        assert_eq!(store.generation, before);
        store.drain();
        assert!(store.generation > before);
        assert_ne!(store.draw(&image, 40, 16).unwrap(), loading);
    }

    #[test]
    fn rapid_viewport_changes_leave_one_job_and_keep_transcript_work() {
        forget_uploads();
        set_mode(ImageMode::Kitty);
        let image = ImageRef::from_source(&png_source(400, 400), ALT).unwrap();
        let mut viewport = ImageViewport::default();
        let area = Rect::new(0, 0, 30, 10);
        draw(&image, 40);
        for _ in 0..100 {
            draw_viewport(&image, &mut viewport, area);
            viewport.handle_key(keys::IMAGE_ZOOM_IN.to_key_event());
        }
        draw_viewport(&image, &mut viewport, area);
        with_store(|store| assert_eq!(store.pending.len(), 2));
        decode_queued();
        with_store(|store| assert_eq!(store.drawn.len(), 2));
        assert!(!take_output().is_empty());
        set_mode(ImageMode::Blocks);
    }

    #[test_case(false; "queued")]
    #[test_case(true; "completed")]
    fn closing_a_viewport_never_uploads_its_stale_result(completed: bool) {
        forget_uploads();
        set_mode(ImageMode::Kitty);
        let image = ImageRef::from_source(&png_source(40, 40), ALT).unwrap();
        let mut viewport = ImageViewport::default();
        draw_viewport(&image, &mut viewport, Rect::new(0, 0, 10, 10));
        if completed {
            decode_queued();
        }
        drop(viewport);
        decode_queued();
        let output = take_output();
        assert!(!output.contains("a=t"));
        assert!(!output.contains("d=A"));
        with_store(|store| assert!(store.drawn.is_empty()));
        set_mode(ImageMode::Blocks);
    }

    #[test]
    fn mode_switch_and_repeated_reset_delete_only_owned_ids() {
        forget_uploads();
        set_mode(ImageMode::Kitty);
        take_output();
        let image = ImageRef::from_source(&png_source(40, 40), ALT).unwrap();
        draw(&image, 40);
        decode_queued();
        assert!(take_output().contains("a=t"));
        set_mode(ImageMode::Blocks);
        forget_uploads();
        let output = take_output();
        assert!(output.contains("a=d,d=I,q=2,i=1"));
        assert!(!output.contains("d=A"));
    }

    #[test]
    fn a_file_that_changes_after_its_header_is_never_uploaded() {
        let dir = workspace_with_picture();
        let path = dir.path().join(PICTURE);
        let image = ImageRef::from_file(&path, ALT).unwrap();
        let mut store = ImageStore::new();
        store.mode = ImageMode::Kitty;
        store.draw(&image, 40, 16);
        fs::write(&path, png_bytes(32, 32)).unwrap();
        store.worker.run_queued();
        store.drain();
        assert!(store.output().is_empty());
        assert!(store.draw(&image, 40, 16).is_none());
    }

    #[test]
    fn evicted_drawings_reuse_their_owned_upload_without_decoding() {
        let image = ImageRef::from_source(&png_source(40, 40), ALT).unwrap();
        let mut store = ImageStore::new();
        store.mode = ImageMode::Kitty;
        store.draw(&image, 40, 16);
        store.worker.run_queued();
        store.drain();
        store.output();
        store.drawn.clear();
        store.draw(&image, 40, 16);
        assert!(store.pending.is_empty());
        assert_eq!(store.kitty.ids(), [1]);
        assert!(store.output().is_empty());
    }

    #[test]
    fn upload_budget_eviction_invalidates_the_old_draw_key() {
        let image = ImageRef::from_source(&png_source(8, 8), ALT).unwrap();
        let mut store = ImageStore::new();
        store.mode = ImageMode::Kitty;
        let mut key = store.key(
            &image,
            ImageLayout { cols: 1, rows: 1 },
            (0, 0, 8, 8),
            ImageSampling::Smooth,
        );
        key.target = (super::MAX_RASTER_PIXELS, 1);
        key.image = super::ImageKey::Bytes(0);
        let first = key.clone();
        for index in 0..32 {
            key.image = super::ImageKey::Bytes(index);
            let id = store.upload(&key, Vec::new(), Ticket::new(None));
            store.drawn.put(key.clone(), Drawn::Kitty(id));
        }
        assert!(!store.drawn.contains(&first));
        assert!(store.kitty.ids().len() <= 16);
        assert!(!store.output.contains("d=A"));
    }

    #[test]
    fn output_flush_cannot_recycle_ids_after_the_generation_check() {
        forget_uploads();
        set_mode(ImageMode::Kitty);
        let image = ImageRef::from_source(&png_source(8, 8), ALT).unwrap();
        with_store(|store| {
            let mut key = store.key(
                &image,
                ImageLayout { cols: 1, rows: 1 },
                (0, 0, 8, 8),
                ImageSampling::Smooth,
            );
            key.image = super::ImageKey::Bytes(0);
            for index in 0..u8::MAX {
                key.image = super::ImageKey::Bytes(u64::from(index));
                let id = store.upload(&key, Vec::new(), Ticket::new(None));
                store.drawn.put(key.clone(), Drawn::Kitty(id));
            }
            store.output();
            store.draw(&image, 40, MAX_IMAGE_ROWS);
        });
        let checked = generation();
        with_store(|store| store.worker.run_queued());
        assert!(take_output().is_empty());
        with_store(|store| {
            assert_eq!(store.generation, checked);
            assert_eq!(store.kitty.ids().last(), Some(&1));
        });
        assert!(generation() > checked);
        assert!(take_output().contains("a=t"));
        with_store(|store| assert_eq!(store.kitty.ids().first(), Some(&1)));
        set_mode(ImageMode::Blocks);
    }
}
