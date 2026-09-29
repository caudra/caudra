use std::mem;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_STRS: [&str; 10] = ["⠋ ", "⠙ ", "⠹ ", "⠸ ", "⠼ ", "⠴ ", "⠦ ", "⠧ ", "⠇ ", "⠏ "];
const SPINNER_FRAME_MS: u128 = 80;

/// How long one glyph stays up. [`crate::repaint::Cadence::SPINNER`] paints at
/// exactly this rate, so no two frames show the same glyph.
pub const SPINNER_FRAME: Duration = Duration::from_millis(SPINNER_FRAME_MS as u64);

pub fn spinner_frame(elapsed_ms: u128) -> char {
    #[cfg(test)]
    let elapsed_ms = test_clock::elapsed(elapsed_ms);
    SPINNER_FRAMES[(elapsed_ms / SPINNER_FRAME_MS) as usize % SPINNER_FRAMES.len()]
}

pub fn spinner_str(elapsed_ms: u128) -> &'static str {
    #[cfg(test)]
    let elapsed_ms = test_clock::elapsed(elapsed_ms);
    SPINNER_STRS[(elapsed_ms / SPINNER_FRAME_MS) as usize % SPINNER_STRS.len()]
}

/// How long a clock that counts while it is drawn has been running. Read
/// through here rather than off the `Instant`, so a test can hold it at one
/// reading however long its render takes.
pub fn live_elapsed(started: Instant) -> Duration {
    let elapsed = started.elapsed();
    #[cfg(test)]
    let elapsed = test_clock::running(elapsed);
    elapsed
}

#[cfg(test)]
pub(crate) mod test_clock {
    use std::{cell::Cell, marker::PhantomData, rc::Rc, time::Duration};

    thread_local! {
        static ELAPSED: Cell<Option<u128>> = const { Cell::new(None) };
        static RUNNING: Cell<Option<Duration>> = const { Cell::new(None) };
    }

    pub(crate) struct FrozenSpinner {
        previous: Option<u128>,
        thread: PhantomData<Rc<()>>,
    }

    impl FrozenSpinner {
        pub(crate) fn at(elapsed_ms: u128) -> Self {
            Self {
                previous: ELAPSED.replace(Some(elapsed_ms)),
                thread: PhantomData,
            }
        }
    }

    impl Drop for FrozenSpinner {
        fn drop(&mut self) {
            ELAPSED.set(self.previous);
        }
    }

    pub(super) fn elapsed(real: u128) -> u128 {
        ELAPSED.get().unwrap_or(real)
    }

    /// Every live clock on this thread reads exactly `elapsed` until this is
    /// dropped, whenever it was started.
    pub(crate) struct FrozenClock {
        previous: Option<Duration>,
        thread: PhantomData<Rc<()>>,
    }

    impl FrozenClock {
        pub(crate) fn at(elapsed: Duration) -> Self {
            Self {
                previous: RUNNING.replace(Some(elapsed)),
                thread: PhantomData,
            }
        }
    }

    impl Drop for FrozenClock {
        fn drop(&mut self) {
            RUNNING.set(self.previous);
        }
    }

    pub(super) fn running(real: Duration) -> Duration {
        RUNNING.get().unwrap_or(real)
    }
}

/// Spinners need a consistent time reference. Using a static epoch avoids
/// passing Instant through every render call.
pub fn animation_elapsed_ms() -> u128 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis()
}

const DEFAULT_MS_PER_CHAR: u64 = 4;
const MIN_DURATION_MS: u64 = 30;
const MAX_DURATION_MS: u64 = 1000;

pub struct Typewriter {
    buffer: String,
    visible_len: usize,
    visible_byte_offset: usize,
    anim_start_visible: usize,
    anim_target: usize,
    anim_start_at: Instant,
    anim_duration: Duration,
    ms_per_char: u64,
}

impl Default for Typewriter {
    fn default() -> Self {
        Self::with_speed(DEFAULT_MS_PER_CHAR)
    }
}

impl Typewriter {
    pub fn new() -> Self {
        Self::with_speed(DEFAULT_MS_PER_CHAR)
    }

    pub fn with_speed(ms_per_char: u64) -> Self {
        Self {
            buffer: String::new(),
            visible_len: 0,
            visible_byte_offset: 0,
            anim_start_visible: 0,
            anim_target: 0,
            anim_start_at: Instant::now(),
            anim_duration: Duration::ZERO,
            ms_per_char,
        }
    }

    pub fn push(&mut self, text: &str) {
        self.buffer.push_str(text);
        self.tick();
        self.anim_start_visible = self.visible_len;
        // Counted from the delta, not recounted over the buffer: a provider
        // that streams token-sized chunks calls this thousands of times per
        // block, and recounting made the reveal cost grow with its own output.
        self.anim_target += text.chars().count();
        if self.ms_per_char == 0 {
            self.advance_visible(self.anim_target);
            return;
        }
        let unrevealed = self.anim_target - self.anim_start_visible;
        let ms = (unrevealed as u64 * self.ms_per_char).clamp(MIN_DURATION_MS, MAX_DURATION_MS);
        self.anim_duration = Duration::from_millis(ms);
        self.anim_start_at = Instant::now();
    }

    pub fn tick(&mut self) {
        if self.visible_len >= self.anim_target {
            return;
        }
        let elapsed = self.anim_start_at.elapsed();
        let progress = (elapsed.as_secs_f64() / self.anim_duration.as_secs_f64()).min(1.0);
        let delta = self.anim_target - self.anim_start_visible;
        let new_len = self.anim_start_visible + (delta as f64 * progress).round() as usize;
        self.advance_visible(new_len);
    }

    pub fn visible(&self) -> &str {
        &self.buffer[..self.visible_byte_offset]
    }

    /// Everything pushed so far, including text the reveal has not reached.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn is_animating(&self) -> bool {
        self.visible_len < self.anim_target
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn buffer_line_count(&self) -> usize {
        if self.buffer.is_empty() {
            0
        } else {
            self.buffer.bytes().filter(|&b| b == b'\n').count() + 1
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.reset_anim();
    }

    pub fn take_all(&mut self) -> String {
        self.reset_anim();
        mem::take(&mut self.buffer)
    }

    #[cfg(test)]
    pub(crate) fn set_buffer(&mut self, text: &str) {
        self.buffer = text.into();
        let len = self.buffer.chars().count();
        self.visible_len = len;
        self.visible_byte_offset = self.buffer.len();
        self.anim_start_visible = len;
        self.anim_target = len;
        self.anim_duration = Duration::ZERO;
    }

    fn reset_anim(&mut self) {
        self.visible_len = 0;
        self.visible_byte_offset = 0;
        self.anim_start_visible = 0;
        self.anim_target = 0;
    }

    fn advance_visible(&mut self, new_len: usize) {
        let skip = new_len - self.visible_len;
        if skip > 0 {
            self.visible_byte_offset = self.buffer[self.visible_byte_offset..]
                .char_indices()
                .nth(skip)
                .map_or(self.buffer.len(), |(i, _)| self.visible_byte_offset + i);
        }
        self.visible_len = new_len;
    }
}

impl PartialEq<&str> for Typewriter {
    fn eq(&self, other: &&str) -> bool {
        self.buffer == *other
    }
}

impl std::fmt::Debug for Typewriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Typewriter")
            .field("buffer", &self.buffer)
            .field("visible_len", &self.visible_len)
            .field("anim_target", &self.anim_target)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_wraps_around() {
        let first = spinner_frame(0);
        let wrapped = spinner_frame(SPINNER_FRAME_MS * SPINNER_FRAMES.len() as u128);
        assert_eq!(first, wrapped);
        assert_ne!(first, spinner_frame(SPINNER_FRAME_MS));
    }

    #[test]
    fn frozen_spinner_controls_both_render_paths_and_restores_time() {
        use super::test_clock::FrozenSpinner;

        {
            let _clock = FrozenSpinner::at(0);
            assert_eq!(spinner_frame(SPINNER_FRAME_MS), SPINNER_FRAMES[0]);
            assert_eq!(spinner_str(SPINNER_FRAME_MS), SPINNER_STRS[0]);
            {
                let _next_frame = FrozenSpinner::at(SPINNER_FRAME_MS);
                assert_eq!(spinner_frame(0), SPINNER_FRAMES[1]);
                assert_eq!(spinner_str(0), SPINNER_STRS[1]);
            }
            assert_eq!(spinner_frame(SPINNER_FRAME_MS), SPINNER_FRAMES[0]);
        }
        assert_eq!(spinner_frame(SPINNER_FRAME_MS), SPINNER_FRAMES[1]);
    }

    /// `Duration::MAX` is a reading no real clock can reach, so seeing it
    /// proves the guard is in force and not seeing it proves it was released.
    #[test]
    fn frozen_clock_holds_live_clocks_and_restores_time() {
        use super::test_clock::FrozenClock;

        let started = Instant::now();
        {
            let _clock = FrozenClock::at(Duration::MAX);
            {
                let _inner = FrozenClock::at(Duration::ZERO);
                assert_eq!(live_elapsed(started), Duration::ZERO);
            }
            assert_eq!(live_elapsed(started), Duration::MAX);
        }
        assert_ne!(live_elapsed(started), Duration::MAX);
    }

    #[test]
    fn push_animates_and_empty_push_is_noop() {
        let mut tw = Typewriter::new();
        tw.push("");
        assert!(!tw.is_animating());
        assert!(tw.is_empty());

        tw.push("hello world, this is a longer string");
        assert_eq!(tw.visible(), "");
        assert!(tw.is_animating());
    }

    #[test]
    fn set_buffer_makes_everything_visible() {
        let mut tw = Typewriter::new();
        tw.set_buffer("héllo 🌍");
        assert_eq!(tw.visible(), "héllo 🌍");
        assert!(!tw.is_animating());
    }

    #[test]
    fn extend_preserves_visible_and_animates_new() {
        let mut tw = Typewriter::new();
        tw.set_buffer("ab");
        tw.push("cdefghijklmnop");
        assert_eq!(tw.visible(), "ab");
        assert!(tw.is_animating());
    }

    #[test]
    fn zero_speed_sequential_pushes_multibyte() {
        let mut tw = Typewriter::with_speed(0);
        tw.push("a");
        tw.push("é");
        tw.push("中");
        tw.push("🦀");
        assert_eq!(tw.visible(), "aé中🦀");
        assert!(!tw.is_animating());
    }

    /// `anim_target` is carried forward per delta instead of recounted, so the
    /// invariant it used to get for free now needs saying: it is the buffer's
    /// char count, across resets and multibyte splits alike.
    #[test]
    fn anim_target_tracks_the_buffer_char_count() {
        const TARGET_MSG: &str = "anim_target must equal the buffer's char count";
        let mut tw = Typewriter::with_speed(0);
        for delta in ["a", "é", "中", "🦀", "", "tail"] {
            tw.push(delta);
            assert_eq!(tw.anim_target, tw.buffer.chars().count(), "{TARGET_MSG}");
        }

        tw.clear();
        tw.push("after clear");
        assert_eq!(tw.anim_target, tw.buffer.chars().count(), "{TARGET_MSG}");

        let _ = tw.take_all();
        tw.push("after take");
        assert_eq!(tw.anim_target, tw.buffer.chars().count(), "{TARGET_MSG}");
    }

    #[test]
    fn clear_and_take_all_reset_byte_offset() {
        let mut tw = Typewriter::with_speed(0);

        tw.push("🔥🔥🔥");
        assert_eq!(tw.visible(), "🔥🔥🔥");
        tw.clear();
        assert!(tw.is_empty());
        assert_eq!(tw.visible(), "");

        tw.push("日本語");
        assert_eq!(tw.visible(), "日本語");
        let taken = tw.take_all();
        assert_eq!(taken, "日本語");
        assert!(tw.is_empty());
        assert_eq!(tw.visible(), "");

        tw.push("ok");
        assert_eq!(tw.visible(), "ok");
    }

    #[test]
    fn set_buffer_then_push_multibyte() {
        let mut tw = Typewriter::with_speed(0);
        tw.set_buffer("àá");
        tw.push("â🎉ã");
        assert_eq!(tw.visible(), "àáâ🎉ã");
    }

    #[test]
    fn repeated_clear_push_cycles() {
        let mut tw = Typewriter::with_speed(0);
        for _ in 0..3 {
            tw.push("🎵test🎵");
            assert_eq!(tw.visible(), "🎵test🎵");
            tw.clear();
            assert_eq!(tw.visible(), "");
        }
    }

    #[test]
    fn partial_eq_compares_full_buffer() {
        let mut tw = Typewriter::new();
        tw.push("hello world, this is enough text");
        assert_eq!(tw, "hello world, this is enough text");
        assert_eq!(tw.visible(), "");
    }
}
