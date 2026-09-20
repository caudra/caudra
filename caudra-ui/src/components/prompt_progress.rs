use std::time::{Duration, Instant};

use caudra_grab::grab_scope;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::components::progress_bar::{self, ProgressBarConfig};
use crate::theme;

pub(crate) const PROMPT_PROGRESS_LABEL: &str = " Processing ";
/// One EWMA over the reported deltas. A raw per-sample rate swings with the
/// server's chunk boundaries and its scheduler, which at the refresh rate of a
/// progress bar reads as a number nobody can look at.
const PROMPT_RATE_SMOOTHING: f64 = 0.3;
/// Under this a sample is mostly timer noise, and dividing by it invents
/// throughput the server never delivered.
const PROMPT_RATE_MIN_SAMPLE: Duration = Duration::from_millis(120);
const PROMPT_RATE_KILO: f64 = 1_000.0;
/// The share of the row the bar itself spends, leaving the rest to the label.
const BAR_WIDTH_RATIO: f64 = 0.1;
/// The run the server matched in its prompt cache, told apart from the run it
/// is prefilling now. Fixed rather than themed: green reads as "already paid
/// for" in every palette.
const CACHE_COLOR: Color = Color::Green;

#[derive(Clone, Copy)]
pub struct PromptProgress {
    pub processed: u32,
    pub total: u32,
    pub cache: u32,
}

/// Prefill reports token counts, never a rate, so the throughput a reader
/// actually wants is derived from the counts as they arrive.
#[derive(Default)]
pub(crate) struct PromptRate {
    baseline: Option<(Instant, u32)>,
    per_second: Option<f64>,
}

impl PromptRate {
    pub(crate) fn sample(&mut self, processed: u32, now: Instant) {
        let Some((measured_at, measured)) = self.baseline else {
            self.baseline = Some((now, processed));
            return;
        };
        let elapsed = now.duration_since(measured_at);
        let advanced = processed.saturating_sub(measured);
        // Holding the baseline instead of moving it lets short frames
        // accumulate into one honest window rather than each being discarded.
        if advanced == 0 || elapsed < PROMPT_RATE_MIN_SAMPLE {
            return;
        }
        self.baseline = Some((now, processed));
        let sample = f64::from(advanced) / elapsed.as_secs_f64();
        self.per_second = Some(match self.per_second {
            Some(smoothed) => smoothed + (sample - smoothed) * PROMPT_RATE_SMOOTHING,
            None => sample,
        });
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// A cached prefix arrives as one jump and finishes before a second
    /// sample exists, so there is deliberately nothing to show for it.
    pub(crate) fn label(&self) -> Option<String> {
        let per_second = self.per_second?;
        Some(if per_second >= PROMPT_RATE_KILO {
            format!(" {:.1}k tok/s ·", per_second / PROMPT_RATE_KILO)
        } else {
            format!(" {per_second:.0} tok/s ·")
        })
    }
}

fn fits(rate: &str, bar_width: u16, width: u16) -> bool {
    let needed = rate.chars().count() + PROMPT_PROGRESS_LABEL.chars().count();
    needed + bar_width as usize <= width as usize
}

/// Draws the prefill bar against the right edge of `area`'s last row and
/// reports the cells it took, so a caller can keep its own content out of
/// them. Shared by the transcript and the side-request modal so the two can
/// never disagree about what a prefill looks like.
pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    progress: PromptProgress,
    rate: &PromptRate,
) -> Option<Rect> {
    if progress.total == 0 || area.width == 0 || area.height == 0 {
        return None;
    }
    let width = area.width;
    let bar_width = (f64::from(width) * BAR_WIDTH_RATIO).round() as u16;
    // The rate is the first thing to go when the terminal is narrow: it is the
    // detail, and the bar is the answer.
    let label = match rate.label() {
        Some(rate) if fits(&rate, bar_width, width) => format!("{rate}{PROMPT_PROGRESS_LABEL}"),
        _ => PROMPT_PROGRESS_LABEL.to_owned(),
    };
    let total_width = label.chars().count() as u16 + bar_width;
    let theme = theme::current();
    let bar_area = Rect::new(
        area.x + width.saturating_sub(total_width),
        area.y + area.height.saturating_sub(1),
        total_width.min(width),
        1,
    );
    grab_scope!("prompt_progress", bar_area);
    progress_bar::render(
        frame,
        bar_area,
        &ProgressBarConfig {
            ratio: f64::from(progress.processed) / f64::from(progress.total),
            style: theme.progress_bar,
            cache_ratio: f64::from(progress.cache) / f64::from(progress.total),
            cache_style: Style::new().fg(CACHE_COLOR),
            label: Some(&label),
            label_style: Some(theme.tool_dim),
            bar_width,
        },
    );
    Some(bar_area)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const RATE_UNSET_MSG: &str = "one sample measures no interval, so there is no rate to show";
    const RATE_HELD_MSG: &str =
        "a frame under the sample window must extend the window, not reset it";
    const BAR_WIDTH: u16 = 8;

    fn rate_at(samples: &[(u32, u64)]) -> Option<String> {
        let start = Instant::now();
        let mut rate = PromptRate::default();
        for (processed, millis) in samples {
            rate.sample(*processed, start + Duration::from_millis(*millis));
        }
        rate.label()
    }

    #[test]
    fn a_single_prompt_progress_frame_reports_no_rate() {
        assert_eq!(rate_at(&[(0, 0)]), None, "{RATE_UNSET_MSG}");
    }

    #[test_case(&[(0, 0), (1_000, 500)] => Some(" 2.0k tok/s ·".to_owned()) ; "first_sample_is_the_measurement")]
    #[test_case(&[(0, 0), (1_000, 500), (3_000, 1_000)] => Some(" 2.6k tok/s ·".to_owned()) ; "later_samples_are_smoothed_toward_the_new_rate")]
    #[test_case(&[(0, 0), (100, 500)] => Some(" 200 tok/s ·".to_owned()) ; "sub_kilo_rates_keep_whole_tokens")]
    fn prompt_rate_reports_observed_throughput(samples: &[(u32, u64)]) -> Option<String> {
        rate_at(samples)
    }

    /// The server reports every chunk boundary, and some land far closer
    /// together than the window. Discarding those would leave a fast prefill
    /// with no rate.
    #[test]
    fn frames_below_the_sample_window_accumulate_into_one_measurement() {
        assert_eq!(
            rate_at(&[(0, 0), (50, 50), (100, 200)]),
            Some(" 500 tok/s ·".to_owned()),
            "{RATE_HELD_MSG}"
        );
    }

    #[test_case(120 => true ; "wide_enough_for_both")]
    #[test_case(20 => false ; "too_narrow_for_the_detail")]
    fn the_rate_yields_to_the_bar_when_the_viewport_is_narrow(width: u16) -> bool {
        fits(" 2.5k tok/s \u{b7}", BAR_WIDTH, width)
    }
}
