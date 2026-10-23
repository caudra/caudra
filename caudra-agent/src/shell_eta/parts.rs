//! A time for each part of a command, so a new chain of known parts gets the
//! sum of their times. Steps joined by `&&`, `||`, `;` or a newline add up; a
//! pipeline lasts as long as its slowest stage. Each finished run's time is
//! shared among its steps in proportion to what they were expected to take,
//! so parts learn from chains as well as from running alone.

use std::collections::HashMap;

use caudra_storage::shell_runs::reading::Part;

pub(super) const FEATURES: usize = 3;
/// What a quick builtin or a filter is assumed to take before it is measured.
const QUICK_PRIOR_MS: f64 = 20.0;
/// How far one run moves a part's log2 time towards what it was given.
const RATE: f64 = 0.5;
const MIN_SHARE_MS: f64 = 1.0;
const UNKNOWN: f32 = -1.0;

#[derive(Clone, Copy)]
struct PartTime {
    log2_ms: f64,
    seen: u32,
}

#[derive(Default)]
pub(super) struct Parts(HashMap<u64, PartTime>);

impl Parts {
    /// The sum of the steps' times and how often the least known part was
    /// measured, or `None` while any part is unknown.
    fn predict(&self, steps: &[Vec<Part>]) -> Option<(f64, u32)> {
        let mut total = 0.0;
        let mut weakest = u32::MAX;
        for step in steps {
            let mut slowest = 0.0f64;
            for part in step {
                let (ms, seen) = self.estimate(*part)?;
                slowest = slowest.max(ms);
                weakest = weakest.min(seen);
            }
            total += slowest;
        }
        Some((total, weakest))
    }

    /// The model's view of the sum: its log2 time, the log2 count of the
    /// least measured part and the number of steps.
    pub(super) fn features(&self, steps: Option<&[Vec<Part>]>) -> [f32; FEATURES] {
        steps
            .and_then(|steps| {
                let (ms, seen) = self.predict(steps)?;
                Some([
                    log2_plus_one(ms),
                    log2_plus_one(f64::from(seen)),
                    steps.len() as f32,
                ])
            })
            .unwrap_or([UNKNOWN; FEATURES])
    }

    pub(super) fn learn(&mut self, steps: &[Vec<Part>], elapsed_ms: u64) {
        let tops: Vec<(Part, Option<f64>)> =
            steps.iter().filter_map(|step| self.slowest(step)).collect();
        let unknown = tops.iter().filter(|(_, ms)| ms.is_none()).count();
        let expected: f64 = tops.iter().filter_map(|(_, ms)| *ms).sum();
        let measured = elapsed_ms as f64;
        for (part, ms) in tops {
            let share = match ms {
                Some(ms) if unknown == 0 && expected > 0.0 => measured * ms / expected,
                Some(ms) => ms,
                None => (measured - expected).max(MIN_SHARE_MS) / unknown as f64,
            };
            let target = share.max(MIN_SHARE_MS).log2();
            self.0
                .entry(part.key)
                .and_modify(|time| {
                    time.log2_ms += RATE * (target - time.log2_ms);
                    time.seen += 1;
                })
                .or_insert(PartTime {
                    log2_ms: target,
                    seen: 1,
                });
        }
    }

    /// The stage a pipeline's time is put down to: one not yet measured, as
    /// its cost is the unknown, else the slowest.
    fn slowest(&self, step: &[Part]) -> Option<(Part, Option<f64>)> {
        let estimates: Vec<(Part, Option<f64>)> = step
            .iter()
            .map(|part| (*part, self.estimate(*part).map(|(ms, _)| ms)))
            .collect();
        estimates
            .iter()
            .find(|(_, ms)| ms.is_none())
            .or_else(|| {
                estimates
                    .iter()
                    .max_by(|a, b| a.1.unwrap_or_default().total_cmp(&b.1.unwrap_or_default()))
            })
            .copied()
    }

    fn estimate(&self, part: Part) -> Option<(f64, u32)> {
        match self.0.get(&part.key) {
            Some(time) => Some((time.log2_ms.exp2(), time.seen)),
            None => part.quick.then_some((QUICK_PRIOR_MS, 1)),
        }
    }
}

fn log2_plus_one(value: f64) -> f32 {
    (value + 1.0).log2() as f32
}

#[cfg(test)]
mod tests {
    use caudra_storage::shell_runs::reading::Part;
    use test_case::test_case;

    use super::{Parts, QUICK_PRIOR_MS};

    const SLEEP: Part = Part {
        key: 1,
        quick: false,
    };
    const BUILD: Part = Part {
        key: 2,
        quick: false,
    };
    const ECHO: Part = Part {
        key: 3,
        quick: true,
    };
    const TAIL: Part = Part {
        key: 4,
        quick: true,
    };
    const SLEEP_MS: u64 = 10_000;
    const BUILD_MS: u64 = 30_000;
    const TOLERANCE_MS: f64 = 1.0;

    fn trained() -> Parts {
        let mut parts = Parts::default();
        parts.learn(&[vec![SLEEP]], SLEEP_MS);
        parts.learn(&[vec![BUILD]], BUILD_MS);
        parts
    }

    #[test_case(&[vec![SLEEP], vec![BUILD]], (SLEEP_MS + BUILD_MS) as f64; "a_chain_adds_up")]
    #[test_case(&[vec![BUILD, TAIL]], BUILD_MS as f64; "a_pipeline_takes_its_slowest_stage")]
    #[test_case(&[vec![SLEEP], vec![ECHO]], SLEEP_MS as f64 + QUICK_PRIOR_MS; "a_quick_part_needs_no_measuring")]
    fn known_parts_predict_their_sum(steps: &[Vec<Part>], expected_ms: f64) {
        let (ms, _) = trained().predict(steps).unwrap();
        assert!(
            (ms - expected_ms).abs() < TOLERANCE_MS,
            "{ms} != {expected_ms}"
        );
    }

    #[test]
    fn an_unknown_part_leaves_no_sum() {
        assert_eq!(Parts::default().predict(&[vec![SLEEP], vec![BUILD]]), None);
    }

    #[test]
    fn a_chain_teaches_its_unknown_part_the_rest() {
        let mut parts = Parts::default();
        parts.learn(&[vec![SLEEP]], SLEEP_MS);
        parts.learn(&[vec![SLEEP], vec![BUILD, TAIL]], SLEEP_MS + BUILD_MS);
        let (ms, _) = parts.predict(&[vec![BUILD]]).unwrap();
        assert!((ms - BUILD_MS as f64).abs() < TOLERANCE_MS, "{ms}");
    }
}
