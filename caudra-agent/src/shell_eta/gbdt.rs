//! Gradient-boosted trees in the style of scikit-learn's `HistGradientBoosting`:
//! numeric columns binned into at most 255 bins, 0/1 token columns kept sparse,
//! trees grown best leaf first with histogram subtraction. A softmax classifier
//! says instant, seconds or minutes; three pinball regressors give the 10th,
//! 50th and 90th percentile of the log2 run time.

use std::panic::resume_unwind;
use std::thread::{self, ScopedJoinHandle};

const MAX_BINS: usize = 255;
const ROUNDS: usize = 150;
const LEAVES: usize = 31;
const LEARNING_RATE: f64 = 0.1;
const MIN_LEAF: usize = 20;
/// Keeps a split from isolating rows the classifier is already sure of.
const MIN_HESSIAN: f64 = 1e-3;
const HESSIAN_FLOOR: f64 = 1e-16;
const LEAF: u16 = u16::MAX;
pub(super) const CLASSES: usize = 3;
pub(super) const QUANTILES: [f64; 3] = [0.1, 0.5, 0.9];

/// One row: numeric columns and the sorted vocabulary ids of its tokens.
pub(super) struct Sample<const N: usize> {
    pub numbers: [f32; N],
    pub tokens: Vec<u16>,
}

impl<const N: usize> Sample<N> {
    fn goes_left(&self, feature: u16, threshold: f32, missing_left: bool) -> bool {
        match usize::from(feature).checked_sub(N) {
            None => {
                let value = self.numbers[usize::from(feature)];
                if value.is_finite() {
                    value <= threshold
                } else {
                    missing_left
                }
            }
            Some(token) => self
                .tokens
                .binary_search(&u16::try_from(token).unwrap_or(LEAF))
                .is_err(),
        }
    }
}

#[derive(Clone, Copy)]
struct Node {
    feature: u16,
    threshold: f32,
    missing_left: bool,
    left: u32,
    right: u32,
    value: f32,
}

const UNSET: Node = Node {
    feature: LEAF,
    threshold: 0.0,
    missing_left: false,
    left: 0,
    right: 0,
    value: 0.0,
};

type Tree = Vec<Node>;
/// A grown tree and the rows that reached each of its leaves.
type Grown = (Tree, Vec<(usize, Vec<u32>)>);

fn predict_tree<const N: usize>(tree: &[Node], sample: &Sample<N>) -> f64 {
    let mut index = 0;
    loop {
        let node = tree[index];
        if node.feature == LEAF {
            return f64::from(node.value);
        }
        index = if sample.goes_left(node.feature, node.threshold, node.missing_left) {
            node.left
        } else {
            node.right
        } as usize;
    }
}

/// Training rows mapped to bins once, shared by every tree of one model.
struct Binned<'a, const N: usize> {
    samples: Vec<&'a Sample<N>>,
    thresholds: Vec<Vec<f32>>,
    offsets: Vec<usize>,
    bins: Vec<u8>,
    tokens: usize,
}

impl<'a, const N: usize> Binned<'a, N> {
    fn new(samples: Vec<&'a Sample<N>>, tokens: usize) -> Self {
        let thresholds: Vec<Vec<f32>> = (0..N)
            .map(|column| thresholds(samples.iter().map(|s| s.numbers[column]).collect()))
            .collect();
        let mut offsets = Vec::with_capacity(N + 1);
        let mut total = 0;
        for column in &thresholds {
            offsets.push(total);
            total += column.len() + 2;
        }
        offsets.push(total);
        let bins = samples
            .iter()
            .flat_map(|sample| {
                thresholds
                    .iter()
                    .zip(sample.numbers)
                    .map(|(column, value)| {
                        let bin = if value.is_finite() {
                            column.partition_point(|t| *t < value)
                        } else {
                            column.len() + 1
                        };
                        u8::try_from(bin).unwrap_or(u8::MAX)
                    })
            })
            .collect();
        Self {
            samples,
            thresholds,
            offsets,
            bins,
            tokens,
        }
    }

    fn histogram_len(&self) -> usize {
        self.offsets[N] + self.tokens
    }

    fn histogram(&self, rows: &[u32], gradients: &[f32], hessians: &[f32]) -> Vec<Cell> {
        let mut histogram = vec![Cell::default(); self.histogram_len()];
        for &row in rows {
            let row = row as usize;
            let cell = Cell {
                gradient: f64::from(gradients[row]),
                hessian: f64::from(hessians[row]),
                count: 1,
            };
            for (column, bin) in self.bins[row * N..(row + 1) * N].iter().enumerate() {
                histogram[self.offsets[column] + usize::from(*bin)].add(cell);
            }
            for token in &self.samples[row].tokens {
                histogram[self.offsets[N] + usize::from(*token)].add(cell);
            }
        }
        histogram
    }

    fn best_split(&self, histogram: &[Cell], total: Cell) -> Option<Split> {
        if (total.count as usize) < 2 * MIN_LEAF {
            return None;
        }
        let parent = total.score();
        let mut best: Option<Split> = None;
        let mut consider = |left: Cell, feature: usize, bin: usize, missing_left: bool| {
            let right = total.minus(left);
            if (left.count as usize) < MIN_LEAF
                || (right.count as usize) < MIN_LEAF
                || left.hessian < MIN_HESSIAN
                || right.hessian < MIN_HESSIAN
            {
                return;
            }
            let gain = left.score() + right.score() - parent;
            if gain > best.map_or(0.0, |split| split.gain) {
                best = Some(Split {
                    gain,
                    feature,
                    bin,
                    missing_left,
                });
            }
        };
        for column in 0..N {
            let cells = &histogram[self.offsets[column]..self.offsets[column + 1]];
            let missing = cells[cells.len() - 1];
            let mut left = Cell::default();
            for (bin, cell) in cells[..cells.len() - 2].iter().enumerate() {
                left.add(*cell);
                if missing.count == 0 {
                    consider(left, column, bin, left.count > total.count - left.count);
                } else {
                    consider(left, column, bin, false);
                    let mut with_missing = left;
                    with_missing.add(missing);
                    consider(with_missing, column, bin, true);
                }
            }
            if missing.count > 0 {
                consider(total.minus(missing), column, cells.len() - 2, false);
            }
        }
        for token in 0..self.tokens {
            let present = histogram[self.offsets[N] + token];
            if present.count > 0 {
                consider(total.minus(present), N + token, 0, false);
            }
        }
        best
    }

    fn goes_left(&self, row: usize, split: Split) -> bool {
        match split.feature.checked_sub(N) {
            None => {
                let bin = usize::from(self.bins[row * N + split.feature]);
                if bin == self.thresholds[split.feature].len() + 1 {
                    split.missing_left
                } else {
                    bin <= split.bin
                }
            }
            Some(token) => self.samples[row]
                .tokens
                .binary_search(&u16::try_from(token).unwrap_or(LEAF))
                .is_err(),
        }
    }

    fn total(&self, histogram: &[Cell]) -> Cell {
        histogram[self.offsets[0]..self.offsets[1]].iter().fold(
            Cell::default(),
            |mut total, cell| {
                total.add(*cell);
                total
            },
        )
    }

    /// Grows one tree and returns it with each leaf's rows; leaf values are
    /// left for the loss to set.
    fn grow(&self, gradients: &[f32], hessians: &[f32]) -> Grown {
        let rows: Vec<u32> = (0..u32::try_from(self.samples.len()).unwrap_or(u32::MAX)).collect();
        let histogram = self.histogram(&rows, gradients, hessians);
        let split = self.best_split(&histogram, self.total(&histogram));
        let mut tree = vec![UNSET];
        let mut leaves = vec![Leaf {
            node: 0,
            rows,
            histogram,
            split,
        }];
        while leaves.len() < LEAVES {
            let Some(pick) = leaves
                .iter()
                .enumerate()
                .filter_map(|(index, leaf)| leaf.split.map(|split| (index, split.gain)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(index, _)| index)
            else {
                break;
            };
            let parent = leaves.swap_remove(pick);
            let Some(split) = parent.split else {
                break;
            };
            let (left_rows, right_rows): (Vec<u32>, Vec<u32>) = parent
                .rows
                .iter()
                .partition(|row| self.goes_left(**row as usize, split));
            let left_smaller = left_rows.len() <= right_rows.len();
            let smaller = self.histogram(
                if left_smaller {
                    &left_rows
                } else {
                    &right_rows
                },
                gradients,
                hessians,
            );
            let mut larger = parent.histogram;
            for (cell, small) in larger.iter_mut().zip(&smaller) {
                *cell = cell.minus(*small);
            }
            let (left_histogram, right_histogram) = if left_smaller {
                (smaller, larger)
            } else {
                (larger, smaller)
            };
            let threshold = self.thresholds.get(split.feature).map_or(0.5, |column| {
                column.get(split.bin).copied().unwrap_or(f32::INFINITY)
            });
            let left = tree.len();
            tree[parent.node] = Node {
                feature: u16::try_from(split.feature).unwrap_or(LEAF),
                threshold,
                missing_left: split.missing_left,
                left: u32::try_from(left).unwrap_or(u32::MAX),
                right: u32::try_from(left + 1).unwrap_or(u32::MAX),
                value: 0.0,
            };
            for (node, rows, histogram) in [
                (left, left_rows, left_histogram),
                (left + 1, right_rows, right_histogram),
            ] {
                tree.push(UNSET);
                let split = self.best_split(&histogram, self.total(&histogram));
                leaves.push(Leaf {
                    node,
                    rows,
                    histogram,
                    split,
                });
            }
        }
        let leaves = leaves
            .into_iter()
            .map(|leaf| (leaf.node, leaf.rows))
            .collect();
        (tree, leaves)
    }
}

/// Up to [`MAX_BINS`] bins: one per distinct value when few, else quantiles.
fn thresholds(mut values: Vec<f32>) -> Vec<f32> {
    values.retain(|value| value.is_finite());
    values.sort_by(f32::total_cmp);
    let mut distinct = values.clone();
    distinct.dedup();
    if distinct.len() <= MAX_BINS {
        return distinct
            .windows(2)
            .map(|pair| (pair[0] + pair[1]) / 2.0)
            .collect();
    }
    let last = values.len() - 1;
    let mut cuts: Vec<f32> = (1..MAX_BINS)
        .map(|bin| {
            let position = bin as f64 / MAX_BINS as f64 * last as f64;
            let low = position.floor() as usize;
            let fraction = position.fract() as f32;
            values[low] + (values[(low + 1).min(last)] - values[low]) * fraction
        })
        .collect();
    cuts.dedup();
    cuts
}

#[derive(Clone, Copy, Default)]
struct Cell {
    gradient: f64,
    hessian: f64,
    count: u32,
}

impl Cell {
    fn add(&mut self, other: Cell) {
        self.gradient += other.gradient;
        self.hessian += other.hessian;
        self.count += other.count;
    }

    fn minus(self, other: Cell) -> Cell {
        Cell {
            gradient: self.gradient - other.gradient,
            hessian: self.hessian - other.hessian,
            count: self.count - other.count,
        }
    }

    fn score(self) -> f64 {
        self.gradient * self.gradient / self.hessian
    }
}

#[derive(Clone, Copy)]
struct Split {
    gain: f64,
    feature: usize,
    bin: usize,
    missing_left: bool,
}

struct Leaf {
    node: usize,
    rows: Vec<u32>,
    histogram: Vec<Cell>,
    split: Option<Split>,
}

struct Classifier {
    prior: [f64; CLASSES],
    rounds: Vec<[Tree; CLASSES]>,
}

impl Classifier {
    fn fit<const N: usize>(binned: &Binned<N>, classes: &[usize]) -> Self {
        let rows = classes.len();
        let mut counts = [0usize; CLASSES];
        for class in classes {
            counts[*class] += 1;
        }
        let prior = counts.map(|count| (count.max(1) as f64 / rows as f64).ln());
        let mut raw = vec![prior; rows];
        let mut gradients = [(); CLASSES].map(|()| vec![0f32; rows]);
        let mut hessians = [(); CLASSES].map(|()| vec![0f32; rows]);
        let mut rounds = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            for (row, class) in classes.iter().enumerate() {
                let probabilities = softmax(raw[row]);
                for k in 0..CLASSES {
                    let target = if *class == k { 1.0 } else { 0.0 };
                    gradients[k][row] = (probabilities[k] - target) as f32;
                    hessians[k][row] =
                        (probabilities[k] * (1.0 - probabilities[k])).max(HESSIAN_FLOOR) as f32;
                }
            }
            let grown: Vec<Grown> = thread::scope(|scope| {
                let handles: Vec<_> = (0..CLASSES)
                    .map(|k| {
                        let (gradients, hessians) = (&gradients[k], &hessians[k]);
                        scope.spawn(move || newton_leaves(binned, gradients, hessians))
                    })
                    .collect();
                handles.into_iter().map(join).collect()
            });
            let trees = grown.into_iter().enumerate().map(|(k, (tree, leaves))| {
                for (node, rows) in leaves {
                    for row in rows {
                        raw[row as usize][k] += f64::from(tree[node].value);
                    }
                }
                tree
            });
            let trees: Vec<Tree> = trees.collect();
            if let Ok(trees) = <[Tree; CLASSES]>::try_from(trees) {
                rounds.push(trees);
            }
        }
        Self { prior, rounds }
    }

    fn predict<const N: usize>(&self, sample: &Sample<N>) -> [f64; CLASSES] {
        let mut raw = self.prior;
        for round in &self.rounds {
            for (k, tree) in round.iter().enumerate() {
                raw[k] += predict_tree(tree, sample);
            }
        }
        softmax(raw)
    }
}

fn newton_leaves<const N: usize>(binned: &Binned<N>, gradients: &[f32], hessians: &[f32]) -> Grown {
    let (mut tree, leaves) = binned.grow(gradients, hessians);
    for (node, rows) in &leaves {
        let (gradient, hessian) = rows.iter().fold((0.0, 0.0), |(g, h), row| {
            (
                g + f64::from(gradients[*row as usize]),
                h + f64::from(hessians[*row as usize]),
            )
        });
        tree[*node].value = (-gradient / hessian * LEARNING_RATE) as f32;
    }
    (tree, leaves)
}

struct Quantile {
    start: f64,
    trees: Vec<Tree>,
}

impl Quantile {
    fn fit<const N: usize>(binned: &Binned<N>, targets: &[f32], quantile: f64) -> Self {
        let rows = targets.len();
        let start = quantile_of(targets.iter().map(|t| f64::from(*t)).collect(), quantile);
        let mut raw = vec![start; rows];
        let hessians = vec![1f32; rows];
        let mut gradients = vec![0f32; rows];
        let mut trees = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            for (row, target) in targets.iter().enumerate() {
                gradients[row] = if f64::from(*target) > raw[row] {
                    -quantile
                } else {
                    1.0 - quantile
                } as f32;
            }
            let (mut tree, leaves) = binned.grow(&gradients, &hessians);
            for (node, rows) in leaves {
                let residuals = rows
                    .iter()
                    .map(|row| f64::from(targets[*row as usize]) - raw[*row as usize])
                    .collect();
                let value = quantile_of(residuals, quantile) * LEARNING_RATE;
                tree[node].value = value as f32;
                for row in rows {
                    raw[row as usize] += value;
                }
            }
            trees.push(tree);
        }
        Self { start, trees }
    }

    fn predict<const N: usize>(&self, sample: &Sample<N>) -> f64 {
        self.start
            + self
                .trees
                .iter()
                .map(|tree| predict_tree(tree, sample))
                .sum::<f64>()
    }
}

fn quantile_of(mut values: Vec<f64>, quantile: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    let rank = (quantile * values.len() as f64).ceil() as usize;
    values[rank.saturating_sub(1).min(values.len().saturating_sub(1))]
}

fn softmax(raw: [f64; CLASSES]) -> [f64; CLASSES] {
    let top = raw.iter().copied().fold(f64::MIN, f64::max);
    let exponentials = raw.map(|value| (value - top).exp());
    let sum: f64 = exponentials.iter().sum();
    exponentials.map(|value| value / sum)
}

fn join<T>(handle: ScopedJoinHandle<'_, T>) -> T {
    handle.join().unwrap_or_else(|panic| resume_unwind(panic))
}

/// What the trees say about one command.
pub(super) struct Estimate {
    pub probabilities: [f64; CLASSES],
    pub log2_ms: [f64; QUANTILES.len()],
}

pub(super) struct Forest {
    classifier: Classifier,
    quantiles: Vec<Quantile>,
}

impl Forest {
    /// `labelled` rows teach the classifier and `timed` ones, the runs that
    /// finished, teach the percentiles. Each model trains on its own thread.
    pub fn fit<const N: usize>(
        labelled: Vec<(&Sample<N>, usize)>,
        timed: Vec<(&Sample<N>, f32)>,
        tokens: usize,
    ) -> Self {
        let (labelled_samples, classes): (Vec<_>, Vec<_>) = labelled.into_iter().unzip();
        let (timed_samples, targets): (Vec<_>, Vec<_>) = timed.into_iter().unzip();
        let timed = Binned::new(timed_samples, tokens);
        thread::scope(|scope| {
            let quantiles: Vec<_> = QUANTILES
                .iter()
                .map(|quantile| {
                    let (timed, targets) = (&timed, &targets);
                    scope.spawn(move || Quantile::fit(timed, targets, *quantile))
                })
                .collect();
            let classifier = Classifier::fit(&Binned::new(labelled_samples, tokens), &classes);
            Self {
                classifier,
                quantiles: quantiles.into_iter().map(join).collect(),
            }
        })
    }

    pub fn estimate<const N: usize>(&self, sample: &Sample<N>) -> Estimate {
        let mut log2_ms = [0.0; QUANTILES.len()];
        for (slot, quantile) in log2_ms.iter_mut().zip(&self.quantiles) {
            *slot = quantile.predict(sample);
        }
        Estimate {
            probabilities: self.classifier.predict(sample),
            log2_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        Binned, Forest, Grown, LEAF, MAX_BINS, MIN_LEAF, QUANTILES, Sample, Split, predict_tree,
    };

    const ROWS: usize = 300;
    const SLOW_TOKEN: u16 = 1;
    const TOKENS: usize = 2;
    const FAST_LOG2_MS: f32 = 4.0;
    const SLOW_LOG2_MS: f32 = 15.0;
    const CONFIDENT: f64 = 0.9;
    const TOLERANCE: f64 = 0.5;
    const LOW: f32 = -2.0;
    const HIGH: f32 = 2.0;
    const NONFINITE: [f32; 3] = [f32::NAN, f32::NEG_INFINITY, f32::INFINITY];
    const TARGET_LEVELS: usize = 10;
    const TARGET_STEP: f32 = 0.1;
    const EQUIVALENCE_TOLERANCE: f64 = 1e-5;

    fn numeric(value: f32) -> Sample<1> {
        Sample {
            numbers: [value],
            tokens: Vec::new(),
        }
    }

    fn assert_training_partition<const N: usize>(binned: &Binned<N>, grown: Grown) {
        let (mut tree, leaves) = grown;
        let mut seen = vec![false; binned.samples.len()];
        for (node, _) in &leaves {
            tree[*node].value = *node as f32;
        }
        for (node, rows) in leaves {
            assert!(rows.len() >= MIN_LEAF);
            for row in rows {
                let row = row as usize;
                assert!(!seen[row]);
                seen[row] = true;
                assert_eq!(predict_tree(&tree, binned.samples[row]), node as f64);
            }
        }
        assert!(seen.into_iter().all(|seen| seen));
    }

    #[test_case(true; "missing_left")]
    #[test_case(false; "missing_right")]
    fn missing_direction_maximizes_gain(missing_left: bool) {
        let samples: Vec<_> = [LOW, HIGH, f32::NAN]
            .into_iter()
            .flat_map(|value| (0..MIN_LEAF).map(move |_| numeric(value)))
            .collect();
        let gradients: Vec<_> = samples
            .iter()
            .map(|sample| {
                if sample.numbers[0] == LOW || (sample.numbers[0].is_nan() && missing_left) {
                    -1.0
                } else {
                    1.0
                }
            })
            .collect();
        let hessians = vec![1.0; samples.len()];
        let binned = Binned::new(samples.iter().collect(), 0);
        let rows: Vec<_> = (0..samples.len() as u32).collect();
        let histogram = binned.histogram(&rows, &gradients, &hessians);
        let split = binned
            .best_split(&histogram, binned.total(&histogram))
            .unwrap();
        assert_eq!(split.missing_left, missing_left);
        assert_eq!(split.bin, 0);
        let expected_gain = (MIN_LEAF * 3) as f64 - MIN_LEAF as f64 / 3.0;
        assert!((split.gain - expected_gain).abs() < f64::EPSILON * expected_gain);
        let grown = binned.grow(&gradients, &hessians);
        assert_eq!(grown.0.len(), 3);
        assert_eq!(grown.0[0].missing_left, missing_left);
        assert_eq!(grown.0[0].threshold, 0.0);
        for value in NONFINITE {
            assert_eq!(numeric(value).goes_left(0, 0.0, missing_left), missing_left);
        }
        assert_training_partition(&binned, grown);
    }

    #[test_case(2, 1, true; "larger_left")]
    #[test_case(1, 2, false; "larger_right")]
    #[test_case(1, 1, false; "tie_right")]
    fn unseen_missing_uses_larger_child(left: usize, right: usize, missing_left: bool) {
        let samples: Vec<_> = [(LOW, left), (HIGH, right)]
            .into_iter()
            .flat_map(|(value, count)| (0..MIN_LEAF * count).map(move |_| numeric(value)))
            .collect();
        let gradients: Vec<_> = samples.iter().map(|sample| sample.numbers[0]).collect();
        let binned = Binned::new(samples.iter().collect(), 0);
        let mut grown = binned.grow(&gradients, &vec![1.0; samples.len()]);
        assert_eq!(grown.0.len(), 3);
        assert_eq!(grown.0[0].missing_left, missing_left);
        grown.0[1].value = LOW;
        grown.0[2].value = HIGH;
        for value in NONFINITE {
            assert_eq!(
                predict_tree(&grown.0, &numeric(value)),
                f64::from(if missing_left { LOW } else { HIGH })
            );
        }
        assert_training_partition(&binned, grown);
    }

    #[test]
    fn constant_observed_values_split_from_missing() {
        let samples: Vec<_> = (0..MIN_LEAF * 2)
            .map(|row| {
                numeric(if row < MIN_LEAF {
                    LOW
                } else {
                    NONFINITE[row % NONFINITE.len()]
                })
            })
            .collect();
        let gradients: Vec<_> = samples
            .iter()
            .map(|sample| {
                if sample.numbers[0].is_finite() {
                    -1.0
                } else {
                    1.0
                }
            })
            .collect();
        let binned = Binned::new(samples.iter().collect(), 0);
        assert!(binned.thresholds[0].is_empty());
        let grown = binned.grow(&gradients, &vec![1.0; samples.len()]);
        assert_eq!(grown.0.len(), 3);
        let root = grown.0[0];
        assert_eq!(root.threshold, f32::INFINITY);
        assert!(!root.missing_left);
        for value in [LOW, HIGH, f32::MIN, f32::MAX] {
            assert!(numeric(value).goes_left(root.feature, root.threshold, root.missing_left));
        }
        assert_training_partition(&binned, grown);
    }

    #[test_case(false; "all_missing")]
    #[test_case(true; "tokens_still_split")]
    fn all_missing_numeric_column_keeps_tokens_independent(with_tokens: bool) {
        let samples: Vec<_> = (0..MIN_LEAF * 2)
            .map(|row| Sample {
                numbers: [NONFINITE[row % NONFINITE.len()]],
                tokens: if with_tokens && row < MIN_LEAF {
                    vec![0]
                } else {
                    Vec::new()
                },
            })
            .collect();
        let gradients: Vec<_> = (0..samples.len())
            .map(|row| if row < MIN_LEAF { -1.0 } else { 1.0 })
            .collect();
        let binned = Binned::new(samples.iter().collect(), 1);
        assert!(binned.thresholds[0].is_empty());
        let grown = binned.grow(&gradients, &vec![1.0; samples.len()]);
        assert_eq!(grown.0[0].feature, if with_tokens { 1 } else { LEAF });
        assert_training_partition(&binned, grown);
    }

    #[test_case(MAX_BINS; "distinct_bins")]
    #[test_case(MAX_BINS * 2; "quantile_bins")]
    fn full_numeric_bin_range_reserves_one_missing_bin(finite: usize) {
        let samples: Vec<_> = (0..finite)
            .map(|value| numeric(value as f32 - finite as f32))
            .chain(NONFINITE.into_iter().map(numeric))
            .collect();
        let binned = Binned::new(samples.iter().collect(), 0);
        let observed = Binned::new(samples[..finite].iter().collect(), 0);
        assert_eq!(binned.thresholds, observed.thresholds);
        assert_eq!(binned.bins[..finite], observed.bins);
        assert_eq!(binned.thresholds[0].len(), MAX_BINS - 1);
        assert!(binned.thresholds[0].iter().all(|value| value.is_finite()));
        assert_eq!(binned.histogram_len(), MAX_BINS + 1);
        assert!(binned.bins[..finite].iter().all(|bin| *bin < u8::MAX));
        assert_eq!(&binned.bins[finite..], &[u8::MAX; NONFINITE.len()]);
        let rows: Vec<_> = (0..samples.len() as u32).collect();
        let histogram =
            binned.histogram(&rows, &vec![1.0; samples.len()], &vec![1.0; samples.len()]);
        assert_eq!(histogram[MAX_BINS].count as usize, NONFINITE.len());
        assert_eq!(binned.total(&histogram).count as usize, samples.len());
        for bin in 0..MAX_BINS {
            let threshold = binned.thresholds[0]
                .get(bin)
                .copied()
                .unwrap_or(f32::INFINITY);
            for missing_left in [false, true] {
                let split = Split {
                    gain: 0.0,
                    feature: 0,
                    bin,
                    missing_left,
                };
                for (row, sample) in samples.iter().enumerate() {
                    assert_eq!(
                        binned.goes_left(row, split),
                        sample.goes_left(0, threshold, missing_left)
                    );
                }
            }
        }
    }

    #[test]
    fn histogram_subtraction_preserves_missing_cells_and_deep_partitions() {
        let values = [LOW, HIGH, f32::NAN];
        let samples: Vec<_> = (0..MIN_LEAF * values.len() * values.len())
            .map(|row| {
                let group = row / MIN_LEAF;
                Sample {
                    numbers: [values[group / values.len()], values[group % values.len()]],
                    tokens: if group.is_multiple_of(2) {
                        vec![0]
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect();
        let gradients: Vec<_> = (0..samples.len())
            .map(|row| (row / MIN_LEAF) as f32)
            .collect();
        let hessians = vec![1.0; samples.len()];
        let rows: Vec<_> = (0..samples.len() as u32).collect();
        let binned = Binned::new(samples.iter().collect(), 1);
        let parent = binned.histogram(&rows, &gradients, &hessians);
        let split = binned.best_split(&parent, binned.total(&parent)).unwrap();
        let (left, right): (Vec<_>, Vec<_>) = rows
            .iter()
            .copied()
            .partition(|row| binned.goes_left(*row as usize, split));
        let left = binned.histogram(&left, &gradients, &hessians);
        let right = binned.histogram(&right, &gradients, &hessians);
        for ((parent, left), right) in parent.iter().zip(&left).zip(&right) {
            let difference = parent.minus(*left);
            assert_eq!(difference.count, right.count);
            assert_eq!(difference.gradient, right.gradient);
            assert_eq!(difference.hessian, right.hessian);
        }
        let grown = binned.grow(&gradients, &hessians);
        assert!(grown.0.len() > 3);
        assert_training_partition(&binned, grown);
    }

    #[test_case(true, false; "missing_left")]
    #[test_case(false, false; "missing_right")]
    #[test_case(false, true; "missing_versus_constant")]
    fn ensembles_learn_numeric_missingness(missing_left: bool, constant: bool) {
        let rows: Vec<_> = (0..ROWS)
            .map(|row| {
                let group = row % 3;
                let value = match group {
                    0 => LOW,
                    1 if !constant => HIGH,
                    _ => NONFINITE[(row / 3) % NONFINITE.len()],
                };
                let fast = value == LOW || (!value.is_finite() && missing_left);
                let (class, target) = if fast {
                    (0, FAST_LOG2_MS)
                } else {
                    (2, SLOW_LOG2_MS)
                };
                let target = target + (row / 3 % TARGET_LEVELS) as f32 * TARGET_STEP;
                (numeric(value), class, target)
            })
            .collect();
        let forest = Forest::fit(
            rows.iter()
                .map(|(sample, class, _)| (sample, *class))
                .collect(),
            rows.iter()
                .map(|(sample, _, target)| (sample, *target))
                .collect(),
            0,
        );
        let observed_rows: Vec<_> = rows
            .iter()
            .map(|(_, class, target)| {
                (
                    numeric(if *class == 0 { LOW } else { HIGH }),
                    *class,
                    *target,
                )
            })
            .collect();
        let observed = Forest::fit(
            observed_rows
                .iter()
                .map(|(sample, class, _)| (sample, *class))
                .collect(),
            observed_rows
                .iter()
                .map(|(sample, _, target)| (sample, *target))
                .collect(),
            0,
        );
        for ((sample, class, _), (observed_sample, _, _)) in rows.iter().zip(&observed_rows) {
            let estimate = forest.estimate(sample);
            let baseline = observed.estimate(observed_sample);
            assert!(
                estimate.probabilities[*class] >= CONFIDENT,
                "class={class}, probabilities={:?}",
                estimate.probabilities
            );
            for (prediction, reference) in estimate
                .probabilities
                .into_iter()
                .zip(baseline.probabilities)
            {
                assert!(
                    (prediction - reference).abs() < EQUIVALENCE_TOLERANCE,
                    "class probabilities: missing={prediction}, finite_baseline={reference}"
                );
            }
            for (index, quantile) in QUANTILES.into_iter().enumerate() {
                let base = if *class == 0 {
                    FAST_LOG2_MS
                } else {
                    SLOW_LOG2_MS
                };
                let expected = f64::from(base)
                    + ((quantile * TARGET_LEVELS as f64).ceil() - 1.0) * f64::from(TARGET_STEP);
                let prediction = estimate.log2_ms[index];
                assert!(
                    (prediction - expected).abs() < TOLERANCE,
                    "missing_left={missing_left}, constant={constant}, value={:?}, expected={expected}, quantile={quantile}, predictions={:?}, finite_baseline={:?}",
                    sample.numbers,
                    estimate.log2_ms,
                    baseline.log2_ms
                );
                assert!(
                    (prediction - baseline.log2_ms[index]).abs() < EQUIVALENCE_TOLERANCE,
                    "quantile={quantile}, missing={prediction}, finite_baseline={}",
                    baseline.log2_ms[index]
                );
            }
        }
    }

    fn sample(row: usize) -> (Sample<1>, usize, f32) {
        let slow = row.is_multiple_of(2);
        let numbers = [if row.is_multiple_of(3) { 1.0 } else { 0.0 }];
        let tokens = if slow { vec![SLOW_TOKEN] } else { Vec::new() };
        let (class, log2_ms) = if slow {
            (2, SLOW_LOG2_MS)
        } else {
            (0, FAST_LOG2_MS)
        };
        (Sample { numbers, tokens }, class, log2_ms)
    }

    #[test]
    fn a_token_that_decides_the_time_is_learned() {
        let rows: Vec<_> = (0..ROWS).map(sample).collect();
        let forest = Forest::fit(
            rows.iter().map(|(s, class, _)| (s, *class)).collect(),
            rows.iter().map(|(s, _, log2_ms)| (s, *log2_ms)).collect(),
            TOKENS,
        );
        for (row, class, log2_ms) in [sample(0), sample(1)] {
            let estimate = forest.estimate(&row);
            assert!(estimate.probabilities[class] >= CONFIDENT);
            assert!((estimate.log2_ms[1] - f64::from(log2_ms)).abs() < TOLERANCE);
        }
    }
}
