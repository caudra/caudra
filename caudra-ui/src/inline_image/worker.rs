//! Decodes and resizes pictures off the UI thread. Threads scale up to
//! `MAX_THREADS` and exit after `IDLE_TIMEOUT` without work, as the highlight
//! pool's do.

use std::fs::File;
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};
use ratatui::text::Line;
use tracing::error;

use crate::image::MAX_IMAGE_BYTES;

use super::blocks::{self, Glyphs};
use super::{CellSize, DrawKey, ImageLayout, ImageMode, ImageOrigin};

const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_THREADS: usize = 2;

pub(super) struct DecodeJob {
    pub key: DrawKey,
    pub origin: ImageOrigin,
    pub layout: ImageLayout,
    pub cell: CellSize,
}

pub(super) enum Payload {
    Lines(Vec<Line<'static>>),
    /// A PNG no larger than the cells it is placed across, for Kitty.
    Png(Vec<u8>),
}

pub(super) struct Decoded {
    pub key: DrawKey,
    pub result: Result<Payload, String>,
}

impl DecodeJob {
    fn run(self) -> Decoded {
        let layout = self.layout;
        let result = load(&self.origin).and_then(|image| match self.key.mode {
            ImageMode::Kitty => {
                let width = (f64::from(layout.cols) * self.cell.width).ceil() as u32;
                let height = (f64::from(layout.rows) * self.cell.height).ceil() as u32;
                let fitted = match image.width() > width || image.height() > height {
                    true => image.resize(width, height, FilterType::Triangle),
                    false => image,
                };
                let mut png = Vec::new();
                fitted
                    .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
                    .map_err(|e| e.to_string())?;
                Ok(Payload::Png(png))
            }
            ImageMode::Blocks | ImageMode::Off => Ok(Payload::Lines(blocks::render(
                &image,
                layout,
                Glyphs::Quadrants,
            ))),
            ImageMode::Sextants => Ok(Payload::Lines(blocks::render(
                &image,
                layout,
                Glyphs::Sextants,
            ))),
        });
        Decoded {
            key: self.key,
            result,
        }
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
        ImageOrigin::Bytes(data) => STANDARD
            .decode(data.as_bytes())
            .map_err(|e| e.to_string())?,
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("image exceeds the size limit".into());
    }
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())
}

struct PoolInner {
    job_rx: flume::Receiver<DecodeJob>,
    result_tx: flume::Sender<Decoded>,
    active_threads: AtomicUsize,
    max_threads: usize,
}

pub(super) struct DecodeWorker {
    job_tx: flume::Sender<DecodeJob>,
    inner: Arc<PoolInner>,
    result_rx: flume::Receiver<Decoded>,
}

impl DecodeWorker {
    pub fn new() -> Self {
        let (job_tx, job_rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::unbounded();
        Self {
            job_tx,
            inner: Arc::new(PoolInner {
                job_rx,
                result_tx,
                active_threads: AtomicUsize::new(0),
                max_threads: if cfg!(test) { 0 } else { MAX_THREADS },
            }),
            result_rx,
        }
    }

    pub fn send(&self, job: DecodeJob) {
        let _ = self.job_tx.send(job);
        self.maybe_spawn_thread();
    }

    pub fn try_recv(&self) -> Option<Decoded> {
        self.result_rx.try_recv().ok()
    }

    /// Tests run with no threads and decode here instead, so nothing they
    /// assert waits on a scheduler.
    #[cfg(test)]
    pub fn run_queued(&self) {
        while let Ok(job) = self.inner.job_rx.try_recv() {
            let _ = self.inner.result_tx.send(job.run());
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
        if inner.result_tx.send(job.run()).is_err() {
            break;
        }
    }
    inner.active_threads.fetch_sub(1, Ordering::AcqRel);
}
