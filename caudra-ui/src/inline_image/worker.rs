//! Decodes and resizes pictures off the UI thread. Threads scale up to
//! `MAX_THREADS` and exit after `IDLE_TIMEOUT` without work, as the highlight
//! pool's do.

use std::fs::File;
use std::io::{Cursor, Read};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use caudra_workbench::ImageSampling;
use image::imageops::{FilterType, overlay, resize};
use image::{DynamicImage, ImageFormat, ImageReader, RgbaImage};
use lru::LruCache;
use ratatui::text::Line;
use tracing::error;

use crate::image::{MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS};

use super::blocks::{self, Glyphs};
use super::{DrawKey, ImageKey, ImageMode, ImageOrigin};

const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_THREADS: usize = 2;
pub(super) const MAX_JOBS: usize = 32;
const SOURCE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const DECODE_ALLOCATION_BYTES: u64 = (MAX_IMAGE_PIXELS * 16) as u64;
const MAX_RESAMPLE_INTERMEDIATE_PIXELS: u64 = MAX_IMAGE_PIXELS as u64;
const SOURCE_CHANGED: &str = "image changed after its header was read";
const SIZE_LIMIT: &str = "image exceeds the size limit";

#[derive(Clone)]
pub(super) struct Ticket {
    cancelled: Arc<AtomicBool>,
    pub owner: Option<Weak<()>>,
}

impl Ticket {
    pub fn new(owner: Option<Weak<()>>) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            owner,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn current(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self
                .owner
                .as_ref()
                .is_none_or(|owner| owner.strong_count() > 0)
    }
}

pub(super) struct DecodeJob {
    pub key: DrawKey,
    pub origin: ImageOrigin,
    pub pixels: (u32, u32),
    pub ticket: Ticket,
}

pub(super) enum Payload {
    Lines(Vec<Line<'static>>),
    /// A PNG no larger than the cells it is placed across, for Kitty.
    Png(Vec<u8>),
}

pub(super) struct Decoded {
    pub key: DrawKey,
    pub result: Result<Payload, String>,
    pub ticket: Ticket,
}

impl DecodeJob {
    fn run(self, sources: &Mutex<SourceCache>) -> Decoded {
        let layout = self.key.layout;
        let result = sources
            .lock()
            .map_err(|e| e.to_string())
            .and_then(|mut sources| sources.load(&self.key.image, &self.origin, self.pixels))
            .and_then(|image| {
                let (x, y, width, height) = self.key.crop;
                let filter = match self.key.sampling {
                    ImageSampling::Smooth => FilterType::CatmullRom,
                    ImageSampling::Nearest => FilterType::Nearest,
                };
                let fitted = resample(
                    image.crop_imm(x, y, width, height),
                    self.key.content,
                    filter,
                );
                let fitted = pad(fitted, self.key.target, self.key.offset);
                match self.key.mode {
                    ImageMode::Kitty => {
                        let mut png = Vec::new();
                        fitted
                            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
                            .map_err(|e| e.to_string())?;
                        Ok(Payload::Png(png))
                    }
                    ImageMode::Blocks | ImageMode::Off => Ok(Payload::Lines(blocks::render(
                        &fitted,
                        layout,
                        Glyphs::Quadrants,
                    ))),
                    ImageMode::Sextants => Ok(Payload::Lines(blocks::render(
                        &fitted,
                        layout,
                        Glyphs::Sextants,
                    ))),
                }
            });
        Decoded {
            key: self.key,
            result,
            ticket: self.ticket,
        }
    }
}

struct SourceCache {
    images: LruCache<ImageKey, Arc<DynamicImage>>,
    bytes: usize,
}

fn resample(image: DynamicImage, target: (u32, u32), filter: FilterType) -> DynamicImage {
    if (image.width(), image.height()) == target {
        return image;
    }
    if transpose_resample((image.width(), image.height()), target) {
        return resample(image.rotate90(), (target.1, target.0), filter).rotate270();
    }
    if filter == FilterType::Nearest || !image.color().has_alpha() {
        return image.resize_exact(target.0, target.1, filter);
    }
    let mut pixels = image.to_rgba32f();
    for pixel in pixels.pixels_mut() {
        let alpha = pixel.0[3];
        for channel in &mut pixel.0[..3] {
            *channel *= alpha;
        }
    }
    let mut fitted = resize(&pixels, target.0, target.1, filter);
    for pixel in fitted.pixels_mut() {
        let alpha = pixel.0[3];
        for channel in &mut pixel.0[..3] {
            *channel = if alpha > 0.0 { *channel / alpha } else { 0.0 };
        }
    }
    DynamicImage::ImageRgba8(DynamicImage::ImageRgba32F(fitted).to_rgba8())
}

fn transpose_resample(source: (u32, u32), target: (u32, u32)) -> bool {
    u64::from(source.0) * u64::from(target.1) > MAX_RESAMPLE_INTERMEDIATE_PIXELS
        && u64::from(source.1) * u64::from(target.0) < u64::from(source.0) * u64::from(target.1)
}

fn pad(image: DynamicImage, target: (u32, u32), offset: (i32, i32)) -> DynamicImage {
    if (image.width(), image.height()) == target && offset == (0, 0) {
        return image;
    }
    let mut canvas = RgbaImage::new(target.0, target.1);
    overlay(
        &mut canvas,
        &image.to_rgba8(),
        i64::from(offset.0),
        i64::from(offset.1),
    );
    DynamicImage::ImageRgba8(canvas)
}

impl SourceCache {
    fn load(
        &mut self,
        key: &ImageKey,
        origin: &ImageOrigin,
        pixels: (u32, u32),
    ) -> Result<Arc<DynamicImage>, String> {
        if !key.current() {
            return Err(SOURCE_CHANGED.into());
        }
        if let Some(image) = self.images.get(key) {
            return Ok(Arc::clone(image));
        }
        let image = Arc::new(load(origin)?);
        if (image.width(), image.height()) != pixels || !key.current() {
            return Err(SOURCE_CHANGED.into());
        }
        let bytes = image.as_bytes().len();
        if bytes <= SOURCE_CACHE_BYTES {
            while self.bytes + bytes > SOURCE_CACHE_BYTES
                || self.images.len() == self.images.cap().get()
            {
                if let Some((_, old)) = self.images.pop_lru() {
                    self.bytes -= old.as_bytes().len();
                }
            }
            self.bytes += bytes;
            self.images.put(key.clone(), Arc::clone(&image));
        }
        Ok(image)
    }
}

pub(super) fn load(origin: &ImageOrigin) -> Result<DynamicImage, String> {
    let bytes = match origin {
        ImageOrigin::File(path) => {
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| {
                    file.take(MAX_IMAGE_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)
                })
                .map_err(|e| format!("{}: {e}", path.display()))?;
            bytes
        }
        ImageOrigin::Bytes(data) => {
            if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
                return Err(SIZE_LIMIT.into());
            }
            STANDARD
                .decode(data.as_bytes())
                .map_err(|e| e.to_string())?
        }
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(SIZE_LIMIT.into());
    }
    let reader = || {
        ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())
    };
    let (width, height) = reader()?.into_dimensions().map_err(|e| e.to_string())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS as u64 {
        return Err(SIZE_LIMIT.into());
    }
    let mut reader = reader()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(DECODE_ALLOCATION_BYTES);
    reader.limits(limits);
    reader.decode().map_err(|e| e.to_string())
}

struct PoolInner {
    job_rx: flume::Receiver<DecodeJob>,
    result_tx: flume::Sender<Decoded>,
    active_threads: AtomicUsize,
    max_threads: usize,
    sources: Mutex<SourceCache>,
}

pub(super) struct DecodeWorker {
    job_tx: flume::Sender<DecodeJob>,
    inner: Arc<PoolInner>,
    result_rx: flume::Receiver<Decoded>,
}

impl DecodeWorker {
    pub fn new() -> Self {
        let (job_tx, job_rx) = flume::bounded(MAX_JOBS);
        let (result_tx, result_rx) = flume::bounded(MAX_JOBS);
        Self {
            job_tx,
            inner: Arc::new(PoolInner {
                job_rx,
                result_tx,
                active_threads: AtomicUsize::new(0),
                max_threads: if cfg!(test) { 0 } else { MAX_THREADS },
                sources: Mutex::new(SourceCache {
                    images: LruCache::new(NonZeroUsize::new(MAX_JOBS).unwrap()),
                    bytes: 0,
                }),
            }),
            result_rx,
        }
    }

    pub fn send(&self, job: DecodeJob) -> bool {
        let mut queued = Vec::new();
        while let Ok(job) = self.inner.job_rx.try_recv() {
            if job.ticket.current() {
                queued.push(job);
            }
        }
        for job in queued {
            let _ = self.job_tx.try_send(job);
        }
        let sent = self.job_tx.try_send(job).is_ok();
        self.maybe_spawn_thread();
        sent
    }

    pub fn try_recv(&self) -> Option<Decoded> {
        if !self.job_tx.is_empty() {
            self.maybe_spawn_thread();
        }
        self.result_rx.try_recv().ok()
    }

    /// Tests run with no threads and decode here instead, so nothing they
    /// assert waits on a scheduler.
    #[cfg(test)]
    pub fn run_queued(&self) {
        while let Ok(job) = self.inner.job_rx.try_recv() {
            if job.ticket.current() {
                let _ = self.inner.result_tx.send(job.run(&self.inner.sources));
            }
        }
    }

    fn maybe_spawn_thread(&self) {
        let current = self.inner.active_threads.load(Ordering::Acquire);
        if current >= self.inner.max_threads
            || self
                .inner
                .active_threads
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let inner = Arc::clone(&self.inner);
        if let Err(e) = thread::Builder::new()
            .name("image-decode".into())
            .spawn(move || worker_loop(&inner))
        {
            self.inner.active_threads.fetch_sub(1, Ordering::AcqRel);
            error!("failed to spawn image decode thread: {e}");
        }
    }
}

fn worker_loop(inner: &PoolInner) {
    while let Ok(job) = inner.job_rx.recv_timeout(IDLE_TIMEOUT) {
        if !job.ticket.current() {
            continue;
        }
        let decoded = job.run(&inner.sources);
        if decoded.ticket.current() && inner.result_tx.send(decoded).is_err() {
            break;
        }
    }
    inner.active_threads.fetch_sub(1, Ordering::AcqRel);
}

#[cfg(test)]
mod tests {
    use super::{
        DecodeJob, DecodeWorker, MAX_RESAMPLE_INTERMEDIATE_PIXELS, Payload, SIZE_LIMIT,
        SOURCE_CACHE_BYTES, Ticket, load, pad, resample, transpose_resample,
    };
    use crate::image::MAX_IMAGE_PIXELS;
    use crate::inline_image::tests::png_source;
    use crate::inline_image::{ImageLayout, ImageMode, ImageOrigin, ImageRef, ImageStore};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use caudra_workbench::ImageSampling;
    use image::imageops::FilterType;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;
    use std::sync::Arc;
    use test_case::test_case;

    const THIN_EDGE: u32 = MAX_IMAGE_PIXELS as u32;
    const THIN_TARGET: (u32, u32) = (32, 16);
    const SOLID_PIXEL: Rgba<u8> = Rgba([80, 120, 160, 255]);

    #[test_case((THIN_EDGE, 1), THIN_TARGET, true; "wide_source_transposes")]
    #[test_case((1, THIN_EDGE), (THIN_TARGET.1, THIN_TARGET.0), false; "tall_source_stays_vertical")]
    fn thin_image_intermediate_stays_within_budget(
        source: (u32, u32),
        target: (u32, u32),
        expected: bool,
    ) {
        let transposed = transpose_resample(source, target);
        assert_eq!(transposed, expected);
        let intermediate = if transposed {
            u64::from(source.1) * u64::from(target.0)
        } else {
            u64::from(source.0) * u64::from(target.1)
        };
        assert!(intermediate <= MAX_RESAMPLE_INTERMEDIATE_PIXELS);
    }

    #[test_case(FilterType::CatmullRom; "smooth")]
    #[test_case(FilterType::Nearest; "nearest")]
    fn resampling_handles_maximum_width_without_a_giant_intermediate(filter: FilterType) {
        let pixels = RgbaImage::from_pixel(THIN_EDGE, 1, SOLID_PIXEL);
        let result = resample(DynamicImage::ImageRgba8(pixels), THIN_TARGET, filter).to_rgba8();
        assert_eq!(result.dimensions(), THIN_TARGET);
        assert!(result.pixels().all(|pixel| *pixel == SOLID_PIXEL));
    }

    #[test_case((100, 100), (104, 112), (2, 6); "partial_terminal_cells")]
    #[test_case((9, 9), (16, 16), (3, 3); "small_image")]
    #[test_case((20, 20), (16, 16), (-2, -2); "partial_zoom_pixels")]
    fn padding_preserves_content_pixels_and_clips_offsets(
        source: (u32, u32),
        target: (u32, u32),
        offset: (i32, i32),
    ) {
        let pixels = RgbaImage::from_pixel(source.0, source.1, SOLID_PIXEL);
        let result = pad(DynamicImage::ImageRgba8(pixels), target, offset).to_rgba8();
        assert_eq!(result.dimensions(), target);
        for (x, y, pixel) in result.enumerate_pixels() {
            let source_x = i64::from(x) - i64::from(offset.0);
            let source_y = i64::from(y) - i64::from(offset.1);
            if (0..i64::from(source.0)).contains(&source_x)
                && (0..i64::from(source.1)).contains(&source_y)
            {
                assert_eq!(*pixel, SOLID_PIXEL);
            } else {
                assert_eq!(pixel.0[3], 0);
            }
        }
    }

    #[test_case(ImageSampling::Nearest; "nearest")]
    #[test_case(ImageSampling::Smooth; "smooth")]
    fn crop_precedes_resampling_and_preserves_alpha(sampling: ImageSampling) {
        let mut pixels = RgbaImage::from_pixel(4, 2, Rgba([255, 0, 0, 255]));
        for y in 0..2 {
            for x in 2..4 {
                pixels.put_pixel(x, y, Rgba([0, 0, 255, 0]));
            }
        }
        let mut bytes = Vec::new();
        pixels
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        let source = caudra_agent::ImageSource::new(
            caudra_agent::ImageMediaType::Png,
            Arc::from(STANDARD.encode(bytes)),
        );
        let image = ImageRef::from_source(&source, "").unwrap();
        let mut store = ImageStore::new();
        store.mode = ImageMode::Kitty;
        let key = store.key(
            &image,
            ImageLayout { cols: 2, rows: 2 },
            (2, 0, 2, 2),
            sampling,
        );
        let worker = DecodeWorker::new();
        worker.send(DecodeJob {
            key,
            origin: image.origin,
            pixels: image.pixels,
            ticket: Ticket::new(None),
        });
        worker.run_queued();
        let Payload::Png(png) = worker.try_recv().unwrap().result.unwrap() else {
            panic!("expected PNG")
        };
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert!(decoded.pixels().all(|pixel| pixel.0[3] == 0));
    }

    #[test]
    fn decoded_sources_are_reused_with_a_byte_budget() {
        let worker = DecodeWorker::new();
        let image = ImageRef::from_source(&png_source(20, 20), "").unwrap();
        let mut sources = worker.inner.sources.lock().unwrap();
        let first = sources
            .load(&image.key, &image.origin, image.pixels)
            .unwrap();
        let second = sources
            .load(&image.key, &image.origin, image.pixels)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(sources.bytes <= SOURCE_CACHE_BYTES);
    }

    #[test]
    fn decoder_rechecks_pixel_limits_without_a_trusted_header() {
        let source = png_source(MAX_IMAGE_PIXELS as u32 + 1, 1);
        let error = load(&ImageOrigin::Bytes(Arc::clone(source.data().unwrap()))).unwrap_err();
        assert_eq!(error, SIZE_LIMIT);
    }

    #[test]
    fn smooth_alpha_does_not_mix_in_invisible_colours() {
        let mut pixels = RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255]));
        pixels.put_pixel(1, 0, Rgba([0, 0, 255, 0]));
        let result = super::resample(
            image::DynamicImage::ImageRgba8(pixels),
            (4, 1),
            super::FilterType::CatmullRom,
        )
        .to_rgba8();
        assert!(
            result
                .pixels()
                .filter(|pixel| pixel.0[3] > 0)
                .all(|pixel| pixel.0[0] == 255 && pixel.0[2] == 0)
        );
    }
}
