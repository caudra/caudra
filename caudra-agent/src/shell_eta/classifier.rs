//! Multinomial logistic regression over hashed features, trained online with
//! AdaGrad: one update per finished run, each costing about a microsecond.

use std::array;

pub(super) const CLASSES: usize = 3;
const SLOT_BITS: u32 = 17;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;
const LEARNING_RATE: f64 = 0.1;
const INITIAL_SQUARE: f32 = 1e-6;
/// Spreads a fixed total weight over a command's features, so a long command
/// does not sound more certain than a short one.
const FEATURE_SCALE: f64 = 2.0;
const TEMPERATURE: f64 = 1.0;
const LOG_FLOOR: f64 = 1e-12;

pub(super) type Probabilities = [f64; CLASSES];

pub(super) struct Classifier {
    weights: Vec<f32>,
    squares: Vec<f32>,
}

impl Classifier {
    pub(super) fn new() -> Self {
        let size = (1 << SLOT_BITS) * CLASSES;
        Self {
            weights: vec![0.0; size],
            squares: vec![INITIAL_SQUARE; size],
        }
    }

    /// Calibrated class probabilities for `features`.
    pub(super) fn predict(&self, features: &[u64]) -> Probabilities {
        let raw = self.raw(&slots(features));
        softmax(raw.map(|p| (p + LOG_FLOOR).ln()), TEMPERATURE)
    }

    pub(super) fn learn(&mut self, features: &[u64], class: usize) {
        let slots = slots(features);
        let raw = self.raw(&slots);
        let value = feature_value(&slots);
        let gradient: Probabilities =
            array::from_fn(|label| (raw[label] - f64::from(u8::from(label == class))) * value);
        for slot in slots {
            for (label, gradient) in gradient.iter().enumerate() {
                let index = slot + label;
                let square = f64::from(self.squares[index]) + gradient * gradient;
                self.squares[index] = square as f32;
                self.weights[index] = (f64::from(self.weights[index])
                    - LEARNING_RATE * gradient / square.sqrt())
                    as f32;
            }
        }
    }

    fn raw(&self, slots: &[usize]) -> Probabilities {
        let value = feature_value(slots);
        let mut scores = [0.0; CLASSES];
        for slot in slots {
            for (label, score) in scores.iter_mut().enumerate() {
                *score += f64::from(self.weights[slot + label]) * value;
            }
        }
        softmax(scores, 1.0)
    }
}

fn slots(features: &[u64]) -> Vec<usize> {
    features
        .iter()
        .map(|hash| usize::try_from(hash & SLOT_MASK).unwrap_or_default() * CLASSES)
        .collect()
}

fn feature_value(slots: &[usize]) -> f64 {
    FEATURE_SCALE / (slots.len().max(1) as f64).sqrt()
}

fn softmax(scores: Probabilities, temperature: f64) -> Probabilities {
    let top = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exp = scores.map(|score| ((score - top) / temperature).exp());
    let total: f64 = exp.iter().sum();
    exp.map(|value| value / total)
}

#[cfg(test)]
mod tests {
    use super::{CLASSES, Classifier};

    const SLOW: [u64; 3] = [1, 2, 3];
    const FAST: [u64; 3] = [4, 5, 6];
    const ROUNDS: usize = 50;

    #[test]
    fn an_untrained_model_is_unsure() {
        let uniform = 1.0 / CLASSES as f64;
        for probability in Classifier::new().predict(&SLOW) {
            assert!((probability - uniform).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn learning_separates_two_commands() {
        let mut classifier = Classifier::new();
        for _ in 0..ROUNDS {
            classifier.learn(&SLOW, 2);
            classifier.learn(&FAST, 0);
        }
        assert!(classifier.predict(&SLOW)[2] > 0.9);
        assert!(classifier.predict(&FAST)[0] > 0.9);
    }
}
