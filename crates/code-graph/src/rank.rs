//! Personalized PageRank over the in-edge CSR, plus the k-hop reach the impact surface reads.
//!
//! # The determinism contract
//!
//! Output is a sorted top-K. A sort has no tolerance band, so the contract is byte-identity: the
//! same graph produces the same ordering on every run, on every machine.
//!
//! Three rules hold it up here:
//!
//! 1. **Fixed contiguous block partitioning.** [`REDUCTION_BLOCK`] is a constant. The obvious
//!    "improvement" is to derive it from the machine — `available_parallelism`, a core count, a
//!    cache probe — so the partition matches the hardware. That change is invisible in review,
//!    passes every test on the machine that makes it, and silently destroys this contract, because
//!    floating-point addition is not associative: a partition that varies by machine changes the
//!    summation tree, which changes the low bits of the dangling-mass reduction, which reorders
//!    ties in a sort that has no tolerance band. Every symptom points away from the cause. Each run
//!    is self-consistent, each reproduces on its own host, and only a diff taken across two
//!    machines shows anything at all.
//! 2. **Every global reduction sums fixed per-block partials in canonical block order.** Never an
//!    accumulating scalar over the whole vector, and never an atomic float add.
//! 3. **The rank vector is `f64`.**
//!
//! Rust has no fast-math flag, so ripwire's fourth rule — compile the PageRank translation unit
//! without floating-point reassociation — has no analogue: rustc never reassociates float
//! arithmetic. That failure mode simply does not exist here.

use crate::{model::NodeId, resolve::Graph};

/// Damping factor. The probability a random surfer follows an edge rather than teleporting.
pub const DAMPING: f64 = 0.85;
/// L1 residual below which the iteration is considered converged.
pub const TOLERANCE: f64 = 1e-6;
/// Iteration ceiling. Reaching it is a truncation and is disclosed as one.
pub const MAX_ITERATIONS: usize = 100;
/// Teleport concentration for a seeded run: the share of teleport mass given to seed nodes.
pub const SEED_CONCENTRATION: f64 = 0.7;

/// Rows summed per partial before partials are combined.
///
/// A compile-time constant, deliberately. See the module docs: deriving this from the hardware is
/// the single easiest way to destroy the determinism contract while every local test stays green.
const REDUCTION_BLOCK: usize = 1024;

/// A rank vector and what the iteration that produced it had to say about itself.
#[derive(Clone, Debug, Default)]
pub struct Ranking {
    pub scores: Vec<f64>,
    /// Power iterations performed.
    pub iterations: usize,
    /// Whether the residual fell below [`TOLERANCE`].
    ///
    /// A converged and a truncated run produce documents that look identical and do not mean the
    /// same thing: the second is a rank vector caught mid-descent, carrying the same scores and the
    /// same ordering. The caller discloses this rather than presenting both as a fixed point.
    pub converged: bool,
    /// L1 residual of the final step.
    pub residual: f64,
}

impl Ranking {
    /// Node ids ordered by descending score, breaking ties by ascending id.
    ///
    /// The id tiebreak is required, not decorative. Near-equal scores would otherwise reorder
    /// between runs and make every top-K and every diff view churn.
    #[must_use]
    pub fn ordered(&self) -> Vec<NodeId> {
        let mut order: Vec<NodeId> = (0..self.scores.len())
            .map(|index| NodeId::try_from(index).unwrap_or(NodeId::MAX))
            .collect();
        order.sort_by(|&left, &right| {
            let left_score = self.scores[left as usize];
            let right_score = self.scores[right as usize];
            right_score
                .partial_cmp(&left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.cmp(&right))
        });
        order
    }
}

/// Where teleport mass goes.
///
/// Personalization lives here and only here. Boosting the initial vector has zero effect at
/// convergence — a power iteration forgets its start — and post-multiplying final ranks by a
/// constant is not personalization either. Bias enters through the teleport vector or it does not
/// enter.
#[derive(Clone, Debug)]
pub enum Teleport {
    /// Every node equally likely.
    Uniform,
    /// [`SEED_CONCENTRATION`] of the mass spread over the seeds, the remainder over everything else.
    Seeded(Vec<NodeId>),
}

/// Runs the power iteration to convergence or the iteration ceiling.
#[must_use]
pub fn pagerank(graph: &Graph, teleport: &Teleport) -> Ranking {
    let nodes = graph.node_count();
    if nodes == 0 {
        return Ranking {
            converged: true,
            ..Ranking::default()
        };
    }

    let prior = teleport_vector(nodes, teleport);
    #[expect(
        clippy::cast_precision_loss,
        reason = "node counts are bounded by the ingest ceiling, far inside f64's exact range"
    )]
    let uniform = 1.0 / nodes as f64;
    let mut rank = vec![uniform; nodes];
    let mut next = vec![0.0f64; nodes];

    let mut iterations = 0;
    let mut residual = f64::INFINITY;
    let mut converged = false;

    while iterations < MAX_ITERATIONS {
        iterations += 1;

        // Dangling mass: rank sitting on nodes with no outgoing edge. Without redistributing it the
        // vector stops being a probability distribution and leaks a little every iteration. Code
        // graphs are mostly sinks — leaf functions, data structs — so the leak is not marginal: the
        // top-K biases toward the dense call core while architecturally central leaf interfaces
        // collapse toward zero. This is the term naive implementations omit.
        let dangling_mass = block_sum(&rank, |index| graph.dangling[index]);

        for (target, slot) in next.iter_mut().enumerate() {
            let target_id = NodeId::try_from(target).unwrap_or(NodeId::MAX);
            // A gather: every write in this loop is sequential and belongs to one target, which is
            // what the in-edge layout bought.
            let mut gathered = 0.0;
            for (source, weight) in graph.in_edges(target_id) {
                let degree = graph.weighted_out_degree[source as usize];
                if degree > 0.0 {
                    gathered += weight * rank[source as usize] / degree;
                }
            }
            *slot = DAMPING.mul_add(
                gathered,
                (DAMPING * dangling_mass + (1.0 - DAMPING)) * prior[target],
            );
        }

        residual = block_sum_map(&rank, &next, |previous, current| (current - previous).abs());
        std::mem::swap(&mut rank, &mut next);
        if residual < TOLERANCE {
            converged = true;
            break;
        }
    }

    Ranking {
        scores: rank,
        iterations,
        converged,
        residual,
    }
}

fn teleport_vector(nodes: usize, teleport: &Teleport) -> Vec<f64> {
    #[expect(
        clippy::cast_precision_loss,
        reason = "node counts are bounded by the ingest ceiling, far inside f64's exact range"
    )]
    let count = nodes as f64;
    match teleport {
        Teleport::Uniform => vec![1.0 / count; nodes],
        Teleport::Seeded(seeds) => {
            let mut seen = vec![false; nodes];
            let mut seed_count = 0usize;
            for &seed in seeds {
                if let Some(slot) = seen.get_mut(seed as usize)
                    && !*slot
                {
                    *slot = true;
                    seed_count += 1;
                }
            }
            if seed_count == 0 || seed_count == nodes {
                return vec![1.0 / count; nodes];
            }
            #[expect(
                clippy::cast_precision_loss,
                reason = "seed counts are bounded by the node count"
            )]
            let seeded = seed_count as f64;
            let rest = count - seeded;
            let on_seed = SEED_CONCENTRATION / seeded;
            let off_seed = (1.0 - SEED_CONCENTRATION) / rest;
            seen.into_iter()
                .map(|is_seed| if is_seed { on_seed } else { off_seed })
                .collect()
        }
    }
}

/// Sums selected entries in fixed contiguous blocks, then sums the partials in block order.
///
/// The two-level shape is the determinism rule, not an optimization. A single accumulating scalar
/// would give a different answer than any blocked version, and a blocked version whose block size
/// varies by machine would give a different answer per machine.
fn block_sum(values: &[f64], mut include: impl FnMut(usize) -> bool) -> f64 {
    let mut partials = Vec::with_capacity(values.len().div_ceil(REDUCTION_BLOCK));
    for (block, chunk) in values.chunks(REDUCTION_BLOCK).enumerate() {
        let base = block * REDUCTION_BLOCK;
        let mut partial = 0.0;
        for (offset, &value) in chunk.iter().enumerate() {
            if include(base + offset) {
                partial += value;
            }
        }
        partials.push(partial);
    }
    partials.iter().sum()
}

fn block_sum_map(left: &[f64], right: &[f64], mut combine: impl FnMut(f64, f64) -> f64) -> f64 {
    let mut partials = Vec::with_capacity(left.len().div_ceil(REDUCTION_BLOCK));
    for block in 0..left.len().div_ceil(REDUCTION_BLOCK) {
        let start = block * REDUCTION_BLOCK;
        let end = (start + REDUCTION_BLOCK).min(left.len());
        let mut partial = 0.0;
        for index in start..end {
            partial += combine(left[index], right[index]);
        }
        partials.push(partial);
    }
    partials.iter().sum()
}

/// The set of symbols reachable from a seed by following edges backwards, up to `hops`.
///
/// Backwards is the point: an edge runs caller to callee, so the symbols affected by changing a
/// callee are the ones that reach it. Returned in ascending id order with the seed excluded.
///
/// This is a **floor**. The graph is name-extracted, so dynamic dispatch, callbacks and
/// macro-generated call sites contribute no edge and cannot appear here.
#[must_use]
pub fn reaching(graph: &Graph, seeds: &[NodeId], hops: usize, limit: usize) -> Vec<NodeId> {
    let nodes = graph.node_count();
    if nodes == 0 || hops == 0 {
        return Vec::new();
    }
    let mut seen = vec![false; nodes];
    let mut frontier: Vec<NodeId> = Vec::new();
    for &seed in seeds {
        if let Some(slot) = seen.get_mut(seed as usize)
            && !*slot
        {
            *slot = true;
            frontier.push(seed);
        }
    }
    let mut reached: Vec<NodeId> = Vec::new();

    for _ in 0..hops {
        let mut next = Vec::new();
        for &target in &frontier {
            for (source, _) in graph.in_edges(target) {
                if let Some(slot) = seen.get_mut(source as usize)
                    && !*slot
                {
                    *slot = true;
                    reached.push(source);
                    next.push(source);
                    if reached.len() >= limit {
                        reached.sort_unstable();
                        return reached;
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    reached.sort_unstable();
    reached
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ingest::{IngestLimits, SourceInput, ingest},
        model::Facts,
        resolve::resolve,
    };

    fn build(files: &[(&str, &str)]) -> (Facts, Graph) {
        let inputs = files
            .iter()
            .map(|(path, source)| SourceInput {
                path: (*path).to_owned(),
                source: (*source).to_owned(),
            })
            .collect();
        let ingested = ingest(inputs, IngestLimits::default());
        let graph = resolve(&ingested.facts);
        (ingested.facts, graph)
    }

    fn node(facts: &Facts, name: &str) -> NodeId {
        facts
            .definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("no definition named {name}"))
            .node
    }

    #[test]
    fn rank_is_a_probability_distribution() {
        // The property the dangling term exists to preserve. Without redistributing sink mass this
        // sum drifts below one a little more every iteration.
        let (_, graph) = build(&[(
            "a.rs",
            "fn leaf() {}\nfn mid() { leaf(); }\nfn top() { mid(); leaf(); }",
        )]);
        let ranking = pagerank(&graph, &Teleport::Uniform);
        let total: f64 = ranking.scores.iter().sum();
        assert!(
            (total - 1.0).abs() < 1e-9,
            "rank mass leaked: total is {total}"
        );
    }

    #[test]
    fn a_called_symbol_outranks_its_caller() {
        let (facts, graph) = build(&[(
            "a.rs",
            "fn leaf() {}\nfn one() { leaf(); }\nfn two() { leaf(); }\nfn three() { leaf(); }",
        )]);
        let ranking = pagerank(&graph, &Teleport::Uniform);
        let leaf = ranking.scores[node(&facts, "leaf") as usize];
        let caller = ranking.scores[node(&facts, "one") as usize];
        assert!(leaf > caller, "leaf {leaf} should outrank caller {caller}");
    }

    #[test]
    fn an_edgeless_graph_converges_to_the_teleport_vector() {
        let (_, graph) = build(&[("a.rs", "fn a() {}\nfn b() {}\nfn c() {}")]);
        let ranking = pagerank(&graph, &Teleport::Uniform);
        assert!(ranking.converged);
        let expected = 1.0 / 3.0;
        for score in &ranking.scores {
            assert!((score - expected).abs() < 1e-9, "{score} != {expected}");
        }
    }

    #[test]
    fn seeding_moves_mass_onto_the_seeds() {
        let (facts, graph) = build(&[("a.rs", "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}")]);
        let seed = node(&facts, "c");
        let uniform = pagerank(&graph, &Teleport::Uniform);
        let seeded = pagerank(&graph, &Teleport::Seeded(vec![seed]));
        assert!(
            seeded.scores[seed as usize] > uniform.scores[seed as usize],
            "the seed should gain mass"
        );
        let total: f64 = seeded.scores.iter().sum();
        assert!((total - 1.0).abs() < 1e-9);
    }

    #[test]
    fn seeding_every_node_is_the_uniform_vector() {
        let (facts, graph) = build(&[("a.rs", "fn a() {}\nfn b() {}")]);
        let all: Vec<_> = facts
            .definitions
            .iter()
            .map(|definition| definition.node)
            .collect();
        let seeded = pagerank(&graph, &Teleport::Seeded(all));
        let uniform = pagerank(&graph, &Teleport::Uniform);
        assert_eq!(seeded.scores, uniform.scores);
    }

    #[test]
    fn ordering_breaks_ties_by_ascending_node_id() {
        // Three symbols with identical scores. Without the id tiebreak this ordering is whatever
        // the sort happened to do, and every top-K churns between runs.
        let ranking = Ranking {
            scores: vec![0.25, 0.25, 0.5, 0.25],
            iterations: 1,
            converged: true,
            residual: 0.0,
        };
        assert_eq!(ranking.ordered(), vec![2, 0, 1, 3]);
    }

    #[test]
    fn convergence_is_reported_rather_than_assumed() {
        let (_, graph) = build(&[("a.rs", "fn leaf() {}\nfn caller() { leaf(); }")]);
        let ranking = pagerank(&graph, &Teleport::Uniform);
        assert!(ranking.converged);
        assert!(ranking.iterations >= 1);
        assert!(ranking.iterations < MAX_ITERATIONS);
        assert!(ranking.residual < TOLERANCE);
    }

    #[test]
    fn an_empty_graph_converges_trivially() {
        let graph = Graph::default();
        let ranking = pagerank(&graph, &Teleport::Uniform);
        assert!(ranking.converged);
        assert!(ranking.scores.is_empty());
    }

    #[test]
    fn ranking_is_bit_identical_across_repeated_runs() {
        // The byte-identity gate. Run the whole pipeline five times over this crate's own sources
        // and require the score bits, not just the ordering, to match. Comparing bit patterns is
        // what catches a reduction whose summation tree moved.
        let files = [
            ("src/rank.rs", include_str!("rank.rs")),
            ("src/resolve.rs", include_str!("resolve.rs")),
            ("src/extract.rs", include_str!("extract.rs")),
            ("src/ingest.rs", include_str!("ingest.rs")),
            ("src/model.rs", include_str!("model.rs")),
        ];
        let run = || {
            let (_, graph) = build(&files);
            let ranking = pagerank(&graph, &Teleport::Uniform);
            let bits: Vec<u64> = ranking.scores.iter().map(|score| score.to_bits()).collect();
            (bits, ranking.ordered(), ranking.iterations)
        };
        let first = run();
        for attempt in 0..4 {
            assert_eq!(first, run(), "run {attempt} diverged");
        }
    }

    #[test]
    fn block_sum_is_independent_of_vector_length_crossing_a_block_boundary() {
        // A guard on the reduction shape itself: summing in fixed blocks must give the same answer
        // regardless of how many blocks the vector happens to fill.
        let values = vec![0.1f64; REDUCTION_BLOCK * 2 + 7];
        let all = block_sum(&values, |_| true);
        let manual: f64 = values
            .chunks(REDUCTION_BLOCK)
            .map(|chunk| chunk.iter().sum::<f64>())
            .sum();
        assert_eq!(all.to_bits(), manual.to_bits());
    }

    #[test]
    fn reaching_walks_edges_backwards_from_the_seed() {
        // `deep` is called by `mid`, which is called by `top`. Changing `deep` reaches both.
        let (facts, graph) = build(&[(
            "a.rs",
            "fn deep() {}\nfn mid() { deep(); }\nfn top() { mid(); }\nfn unrelated() {}",
        )]);
        let deep = node(&facts, "deep");
        let one_hop = reaching(&graph, &[deep], 1, 100);
        assert_eq!(one_hop, vec![node(&facts, "mid")]);

        let two_hops = reaching(&graph, &[deep], 2, 100);
        let mut expected = vec![node(&facts, "mid"), node(&facts, "top")];
        expected.sort_unstable();
        assert_eq!(two_hops, expected);
        assert!(!two_hops.contains(&node(&facts, "unrelated")));
        assert!(!two_hops.contains(&deep), "the seed is excluded");
    }

    #[test]
    fn reaching_respects_its_limit_and_stays_sorted() {
        let (facts, graph) = build(&[(
            "a.rs",
            "fn t() {}\nfn a() { t(); }\nfn b() { t(); }\nfn c() { t(); }",
        )]);
        let reached = reaching(&graph, &[node(&facts, "t")], 3, 2);
        assert_eq!(reached.len(), 2);
        let mut sorted = reached.clone();
        sorted.sort_unstable();
        assert_eq!(reached, sorted);
    }

    #[test]
    fn reaching_terminates_on_a_cycle() {
        let (facts, graph) = build(&[("a.rs", "fn a() { b(); }\nfn b() { a(); }")]);
        let reached = reaching(&graph, &[node(&facts, "a")], 50, 100);
        assert_eq!(reached, vec![node(&facts, "b")]);
    }
}
