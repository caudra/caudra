//! What each model's tokenizer makes of the text o200k counts, learned from
//! what its provider billed.
//!
//! The local estimate is exact only for OpenAI's own models. A request the
//! provider bills is the one honest count of another tokenizer, and the
//! difference between two consecutive requests of one conversation is what
//! the text appended between them cost. Comparing that with the estimate of
//! the same text gives one sample of the model's ratio.
//!
//! The registry is process-wide because a model's tokenizer does not depend on
//! the session or the subagent asking. It is seeded from storage once and
//! learns for as long as the process runs.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use caudra_storage::StateDir;
use caudra_storage::token_calibration::{StoredCalibration, TokenCalibrationStore};
use tracing::warn;

use crate::model::Model;
use crate::tokens::TokenScale;

/// Below this many estimated tokens, a few tokens of framing the estimate
/// cannot see dominate the sample.
pub const MIN_SAMPLE_TOKENS: u32 = 256;
/// One sample's ratio outside this is a measurement gone wrong rather than a
/// tokenizer: a provider that dropped or added content between the requests.
const SAMPLE_RATIO_MIN: f64 = 0.25;
const SAMPLE_RATIO_MAX: f64 = 4.0;
/// The ratio cards and compaction use stays inside this, whatever was learned.
const RATIO_MIN: f64 = 0.5;
const RATIO_MAX: f64 = 2.0;
/// How many later samples halve an older sample's weight.
const HALF_LIFE_SAMPLES: f64 = 20.0;

type Registry = HashMap<String, HashMap<String, Calibration>>;

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(RwLock::default);

/// A model's learned ratio of billed tokens to estimated ones.
///
/// `weight` is the decayed size of the samples behind `ratio`, so a large
/// sample moves it further than a small one and old samples fade.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Calibration {
    pub ratio: f64,
    pub weight: f64,
    pub samples: u64,
}

impl Calibration {
    /// Folds in what `estimated` o200k tokens were billed as. `false` leaves
    /// the calibration untouched for a sample too small or too far off to
    /// trust.
    pub fn observe(&mut self, estimated: u32, reported: u32) -> bool {
        if estimated < MIN_SAMPLE_TOKENS {
            return false;
        }
        let estimated = f64::from(estimated);
        let sample = f64::from(reported) / estimated;
        if !(SAMPLE_RATIO_MIN..=SAMPLE_RATIO_MAX).contains(&sample) {
            return false;
        }
        self.weight = self.weight * decay() + estimated;
        self.ratio += (sample - self.ratio) * estimated / self.weight;
        self.samples += 1;
        true
    }

    /// The ratio as estimates apply it. Only this is clamped: the learned
    /// value stays raw, so a clamp never feeds back into learning.
    pub fn scale(&self) -> TokenScale {
        TokenScale::new(self.ratio.clamp(RATIO_MIN, RATIO_MAX))
    }
}

fn decay() -> f64 {
    0.5_f64.powf(HALF_LIFE_SAMPLES.recip())
}

/// Loads what earlier runs learned. A model already learned in this process
/// keeps what it has, which is newer.
fn seed(rows: impl IntoIterator<Item = StoredCalibration>) {
    let Ok(mut registry) = REGISTRY.write() else {
        return;
    };
    for row in rows {
        registry
            .entry(row.provider)
            .or_default()
            .entry(row.model)
            .or_insert(Calibration {
                ratio: row.ratio,
                weight: row.weight,
                samples: row.samples,
            });
    }
}

/// Seeds from what earlier runs stored. A store that cannot be read leaves
/// every model at the unit scale, which is what estimates were before.
pub fn load_from_storage(dir: &StateDir) {
    match TokenCalibrationStore::open(dir).and_then(|store| store.load_all()) {
        Ok(rows) => seed(rows),
        Err(error) => warn!(%error, "token calibration unavailable; estimates stay uncalibrated"),
    }
}

pub fn scale(model: &Model) -> TokenScale {
    scale_of(&model.provider, &model.id)
}

/// The scale for a model named the way history records it.
pub fn scale_of(provider: &str, model: &str) -> TokenScale {
    REGISTRY
        .read()
        .ok()
        .and_then(|registry| Some(registry.get(provider)?.get(model)?.scale()))
        .unwrap_or_default()
}

/// Learns one sample for `model`, returning the row to persist when it was
/// accepted.
pub fn observe(model: &Model, estimated: u32, reported: u32) -> Option<StoredCalibration> {
    let mut registry = REGISTRY.write().ok()?;
    let calibration = registry
        .entry(model.provider.to_string())
        .or_default()
        .entry(model.id.clone())
        .or_default();
    calibration
        .observe(estimated, reported)
        .then(|| StoredCalibration {
            provider: model.provider.to_string(),
            model: model.id.clone(),
            ratio: calibration.ratio,
            weight: calibration.weight,
            samples: calibration.samples,
            updated_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64),
        })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{Calibration, HALF_LIFE_SAMPLES, MIN_SAMPLE_TOKENS, RATIO_MAX, RATIO_MIN, decay};
    use crate::tokens::TokenScale;

    const SAMPLE: u32 = 1_000;
    const TOLERANCE: f64 = 1e-9;
    const FIRST_SETS: &str = "the first sample sets the ratio outright";
    const REJECTED: &str = "a rejected sample leaves the calibration untouched";
    const HALFWAY: &str = "twenty samples at a new ratio move it about halfway";
    const LARGER_MOVES_MORE: &str = "a larger sample moves the ratio further";
    const CLAMPED: &str = "the applied scale is clamped while the learned ratio is not";

    fn learned(ratio: f64, samples: usize) -> Calibration {
        let mut calibration = Calibration::default();
        for _ in 0..samples {
            calibration.observe(SAMPLE, (f64::from(SAMPLE) * ratio) as u32);
        }
        calibration
    }

    #[test]
    fn a_sample_halves_after_its_half_life() {
        assert!((decay().powf(HALF_LIFE_SAMPLES) - 0.5).abs() < TOLERANCE);
    }

    #[test]
    fn the_first_sample_sets_the_ratio() {
        let mut calibration = Calibration::default();
        assert!(calibration.observe(SAMPLE, 1_250));
        assert!((calibration.ratio - 1.25).abs() < TOLERANCE, "{FIRST_SETS}");
        assert_eq!(calibration.samples, 1);
    }

    #[test_case(MIN_SAMPLE_TOKENS - 1, MIN_SAMPLE_TOKENS ; "too_small")]
    #[test_case(SAMPLE, SAMPLE * 5 ; "too_high")]
    #[test_case(SAMPLE, SAMPLE / 5 ; "too_low")]
    fn an_untrustworthy_sample_is_rejected(estimated: u32, reported: u32) {
        let mut calibration = learned(1.1, 3);
        let before = calibration;
        assert!(!calibration.observe(estimated, reported));
        assert_eq!(calibration, before, "{REJECTED}");
    }

    #[test]
    fn twenty_samples_at_a_new_ratio_move_it_about_halfway() {
        let mut calibration = learned(1.0, 200);
        for _ in 0..20 {
            calibration.observe(SAMPLE, 1_400);
        }
        assert!(
            (calibration.ratio - 1.2).abs() < 0.02,
            "{HALFWAY}: {}",
            calibration.ratio
        );
    }

    #[test]
    fn a_larger_sample_moves_the_ratio_further() {
        let mut small = learned(1.0, 50);
        let mut large = small;
        small.observe(SAMPLE, SAMPLE * 2);
        large.observe(SAMPLE * 10, SAMPLE * 20);
        assert!(large.ratio > small.ratio, "{LARGER_MOVES_MORE}");
    }

    #[test_case(0.3, RATIO_MIN ; "below")]
    #[test_case(1.3, 1.3 ; "inside")]
    #[test_case(3.5, RATIO_MAX ; "above")]
    fn the_applied_scale_is_clamped(ratio: f64, applied: f64) {
        let calibration = Calibration {
            ratio,
            weight: f64::from(SAMPLE),
            samples: 1,
        };
        assert_eq!(calibration.scale(), TokenScale::new(applied), "{CLAMPED}");
        assert_eq!(calibration.ratio, ratio, "{CLAMPED}");
    }
}
