//! Recent completed runs indexed by their feature hashes. A query is answered
//! from the most similar runs by IDF-weighted Jaccard similarity, so a command
//! seen before, or one close to it, gets the time it actually took.

use std::collections::{HashMap, VecDeque};

const POSTINGS_PER_FEATURE: usize = 400;
/// The rarest query features whose postings supply the candidates.
const QUERY_FEATURES: usize = 6;
const NEIGHBOURS: usize = 8;
const MIN_SIMILARITY: f64 = 0.2;
const MEDIAN: f64 = 0.5;

pub(super) struct Estimate {
    pub(super) log2_ms: f64,
    pub(super) similarity: f64,
}

pub(super) struct Neighbours {
    capacity: usize,
    first_id: u64,
    runs: VecDeque<(Vec<u64>, f64)>,
    document_frequency: HashMap<u64, u32>,
    postings: HashMap<u64, VecDeque<u64>>,
}

impl Neighbours {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            first_id: 0,
            runs: VecDeque::new(),
            document_frequency: HashMap::new(),
            postings: HashMap::new(),
        }
    }

    pub(super) fn estimate(&self, query: &[u64]) -> Option<Estimate> {
        let seen = self.runs.len() as f64 + 1.0;
        let weights: HashMap<u64, f64> = query
            .iter()
            .map(|feature| (*feature, self.idf(*feature, seen)))
            .collect();
        let query_weight: f64 = weights.values().sum();
        let mut ranked: Vec<(u64, f64)> = weights.iter().map(|(f, w)| (*f, *w)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut candidates: Vec<u64> = ranked
            .iter()
            .take(QUERY_FEATURES)
            .filter_map(|(feature, _)| self.postings.get(feature))
            .flatten()
            .copied()
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        let mut scored: Vec<(f64, u64)> = candidates
            .into_iter()
            .filter_map(|id| {
                let (features, _) = self.run(id)?;
                let (shared, extra) =
                    features
                        .iter()
                        .fold((0.0, 0.0), |(shared, extra), feature| {
                            match weights.get(feature) {
                                Some(weight) => (shared + weight, extra),
                                None => (shared, extra + self.idf(*feature, seen)),
                            }
                        });
                Some((shared / (query_weight + extra), id))
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
        let mut top: Vec<(f64, f64)> = scored
            .into_iter()
            .take(NEIGHBOURS)
            .filter(|(similarity, _)| *similarity > MIN_SIMILARITY)
            .filter_map(|(similarity, id)| Some((self.run(id)?.1, similarity)))
            .collect();
        let similarity = top.first()?.1;
        top.sort_by(|a, b| a.0.total_cmp(&b.0));
        Some(Estimate {
            log2_ms: weighted_quantile(&top, MEDIAN),
            similarity,
        })
    }

    pub(super) fn insert(&mut self, features: Vec<u64>, log2_ms: f64) {
        let id = self.first_id + self.runs.len() as u64;
        for feature in &features {
            *self.document_frequency.entry(*feature).or_default() += 1;
            let list = self.postings.entry(*feature).or_default();
            list.push_back(id);
            if list.len() > POSTINGS_PER_FEATURE {
                list.pop_front();
            }
        }
        self.runs.push_back((features, log2_ms));
        if self.runs.len() > self.capacity {
            self.evict();
        }
    }

    fn run(&self, id: u64) -> Option<&(Vec<u64>, f64)> {
        self.runs
            .get(usize::try_from(id.checked_sub(self.first_id)?).ok()?)
    }

    fn idf(&self, feature: u64, seen: f64) -> f64 {
        let frequency = self
            .document_frequency
            .get(&feature)
            .copied()
            .unwrap_or_default();
        (seen / (f64::from(frequency) + 1.0)).ln() + 1.0
    }

    fn evict(&mut self) {
        let Some((features, _)) = self.runs.pop_front() else {
            return;
        };
        let id = self.first_id;
        self.first_id += 1;
        for feature in features {
            if let Some(count) = self.document_frequency.get_mut(&feature) {
                *count -= 1;
                if *count == 0 {
                    self.document_frequency.remove(&feature);
                }
            }
            if let Some(list) = self.postings.get_mut(&feature) {
                if list.front() == Some(&id) {
                    list.pop_front();
                }
                if list.is_empty() {
                    self.postings.remove(&feature);
                }
            }
        }
    }
}

/// The first value whose cumulative weight reaches `quantile`.
fn weighted_quantile(sorted: &[(f64, f64)], quantile: f64) -> f64 {
    let total: f64 = sorted.iter().map(|(_, weight)| weight).sum();
    let mut cumulative = 0.0;
    for (value, weight) in sorted {
        cumulative += weight;
        if cumulative / total >= quantile {
            return *value;
        }
    }
    sorted.last().map_or(0.0, |(value, _)| *value)
}

#[cfg(test)]
mod tests {
    use super::Neighbours;

    const BUILD: [u64; 3] = [10, 11, 12];
    const BUILD_LOG2_MS: f64 = 15.0;
    const CAPACITY: usize = 2;

    #[test]
    fn an_empty_index_knows_nothing() {
        assert!(Neighbours::new(CAPACITY).estimate(&BUILD).is_none());
    }

    #[test]
    fn a_repeat_gets_its_own_time() {
        let mut index = Neighbours::new(CAPACITY);
        index.insert(BUILD.to_vec(), BUILD_LOG2_MS);
        index.insert(vec![20, 21], 3.0);
        let estimate = index.estimate(&BUILD).unwrap();
        assert!((estimate.similarity - 1.0).abs() < f64::EPSILON);
        assert!((estimate.log2_ms - BUILD_LOG2_MS).abs() < f64::EPSILON);
    }

    #[test]
    fn eviction_forgets_the_oldest_run() {
        let mut index = Neighbours::new(CAPACITY);
        index.insert(BUILD.to_vec(), BUILD_LOG2_MS);
        index.insert(vec![20, 21], 3.0);
        index.insert(vec![30, 31], 4.0);
        assert!(index.estimate(&BUILD).is_none());
    }
}
