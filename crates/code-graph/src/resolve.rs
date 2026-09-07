//! Turns references into weighted edges.
//!
//! This is the approximate part, and it is approximate on purpose. tree-sitter is syntax-only: no
//! types, no name resolution, no cross-file knowledge. Linking a reference to the definition it
//! means is undecidable without a per-language semantic analyzer, which this crate does not have
//! and does not pretend to have.
//!
//! The deliverable is an importance *ranking*, not a sound call graph. False edges are expected.
//! What is not acceptable is a false edge that looks like a certain one, so every approximation
//! here is either bounded by a rule that makes it sound, or disclosed.

use std::collections::HashMap;

use workcell_source_languages::ReferenceKind;

use crate::model::{Facts, NodeId, Reference};

/// How a reference was matched to its target, in descending confidence.
///
/// The tier is not decoration: it becomes the reference's contribution to the edge weight, so a
/// guess and a certainty do not push the ranking equally hard.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResolutionTier {
    /// The syntax named the target unambiguously. Only shipped where a language makes it sound.
    Precise,
    /// A definition in the same file.
    SameFile,
    /// A definition in the same directory.
    SameDirectory,
    /// The single same-language definition anywhere in the tree.
    UniqueGlobal,
}

impl ResolutionTier {
    /// Base confidence contributed by one reference resolved at this tier.
    #[must_use]
    pub const fn confidence(self) -> f64 {
        match self {
            Self::Precise => 1.0,
            Self::SameFile => 0.9,
            Self::SameDirectory => 0.7,
            Self::UniqueGlobal => 0.5,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Precise => "precise",
            Self::SameFile => "same-file",
            Self::SameDirectory => "same-directory",
            Self::UniqueGlobal => "unique-global",
        }
    }
}

/// A name defined in so many places that matching it says almost nothing.
///
/// `new`, `build`, `get`, `run`. Ripwire measured this threshold; the deboost keeps a common name
/// from dominating a ranking through sheer multiplicity rather than importance.
const OVER_COMMON_DEFINITIONS: usize = 16;
const OVER_COMMON_DEBOOST: f64 = 0.5;
/// A leading underscore is the near-universal spelling for "private to this module".
const PRIVATE_DEBOOST: f64 = 0.8;
/// Edge weight ceiling. Bounds the tail so one hot call site cannot swamp the graph.
const MAX_EDGE_WEIGHT: f64 = 8.0;

/// One resolved edge, before it is laid out as CSR.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub weight: f64,
    /// References that contributed. The weight's sublinear term.
    pub references: u32,
}

/// What resolution could not do, kept so the caller can disclose it.
#[derive(Clone, Debug, Default)]
pub struct ResolutionDiagnostics {
    /// References that resolved to more than one candidate at the chosen tier.
    ///
    /// The resolver does not pick one. It splits the weight `1/k` across all of them, which
    /// tolerates the ambiguity instead of inventing a tiebreak.
    pub ambiguous: usize,
    /// References whose name matched no compatible definition anywhere.
    pub unresolved: usize,
    /// Per-symbol ambiguity counts, for the `amb=` annotation.
    pub ambiguity_by_node: HashMap<NodeId, u32>,
    /// Edges dropped because a symbol referenced itself.
    pub self_loops_dropped: usize,
}

/// The resolved call graph in in-edge compressed sparse row form.
///
/// Keyed by **target**, not source. PageRank propagates rank along incoming edges, so the power
/// iteration multiplies the transpose; an in-edge layout makes that a per-row gather with one
/// sequential write per target. A source-keyed layout would force a scatter with random writes and
/// write races, which is the opposite of what the iteration wants.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    /// Per-target start offsets, length `nodes + 1`.
    pub row_offsets: Vec<u32>,
    /// Source node of each in-edge.
    pub col_indices: Vec<NodeId>,
    /// Weight of each in-edge, parallel to `col_indices`.
    pub values: Vec<f64>,
    /// Weighted out-degree per source, for the `1/outdeg` normalization.
    pub weighted_out_degree: Vec<f64>,
    /// Sources with no outgoing edge. Their rank mass has nowhere to flow and must be redistributed.
    pub dangling: Vec<bool>,
    pub diagnostics: ResolutionDiagnostics,
}

impl Graph {
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.row_offsets.len().saturating_sub(1)
    }

    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.col_indices.len()
    }

    /// Out-edge targets per source, built by transposing the in-edge CSR.
    ///
    /// The CSR is keyed by target because that is what the power iteration wants. Answering "what
    /// does this symbol call" is the other direction, and it is a one-pass transpose rather than a
    /// second stored layout: keeping both in the graph would double the memory every ranking run
    /// pays for, to serve a question only the reference tools ask.
    ///
    /// Each row is ascending, so a caller reading it produces the same order on every run.
    #[must_use]
    pub fn out_adjacency(&self) -> Vec<Vec<NodeId>> {
        let mut out: Vec<Vec<NodeId>> = vec![Vec::new(); self.node_count()];
        for target in 0..self.node_count() {
            let Ok(target_id) = NodeId::try_from(target) else {
                continue;
            };
            for (source, _) in self.in_edges(target_id) {
                if let Some(row) = out.get_mut(source as usize) {
                    row.push(target_id);
                }
            }
        }
        // The CSR is ascending by target within a row, so pushes arrive ascending per source
        // already; the sort is cheap insurance against that layout changing.
        for row in &mut out {
            row.sort_unstable();
            row.dedup();
        }
        out
    }

    /// The in-edges of one target: `(source, weight)` pairs.
    pub fn in_edges(&self, target: NodeId) -> impl Iterator<Item = (NodeId, f64)> + '_ {
        let index = target as usize;
        let (start, end) = match (self.row_offsets.get(index), self.row_offsets.get(index + 1)) {
            (Some(&start), Some(&end)) => (start as usize, end as usize),
            _ => (0, 0),
        };
        self.col_indices[start..end]
            .iter()
            .copied()
            .zip(self.values[start..end].iter().copied())
    }
}

/// An index from name to the definitions that spell it.
struct NameIndex {
    by_name: HashMap<String, Vec<NodeId>>,
}

impl NameIndex {
    fn build(facts: &Facts) -> Self {
        let mut by_name: HashMap<String, Vec<NodeId>> = HashMap::new();
        for definition in &facts.definitions {
            // A section is data, not a call target. Indexing one would let a YAML key answer a
            // function call that happens to share its spelling.
            if !definition.kind.callable() {
                continue;
            }
            by_name
                .entry(definition.name.clone())
                .or_default()
                .push(definition.node);
        }
        for nodes in by_name.values_mut() {
            nodes.sort_unstable();
        }
        Self { by_name }
    }

    fn candidates(&self, name: &str) -> &[NodeId] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }
}

/// Resolves every call reference and lays the result out as an in-edge CSR.
///
/// Non-call references are not edges. An import or an inheritance clause is a use site with its own
/// `file:line`, reported by the refs surface, but it does not describe control flow and minting an
/// edge from one would inflate every ranking.
#[must_use]
pub fn resolve(facts: &Facts) -> Graph {
    let index = NameIndex::build(facts);
    let node_count = facts.definitions.len();
    let mut diagnostics = ResolutionDiagnostics::default();

    // Accumulated per (source, target) pair rather than appended per reference. A duplicate pair
    // was never a second entry to merge later; it is the reference count in the weight formula.
    let mut accumulator: HashMap<(NodeId, NodeId), (f64, u32)> = HashMap::new();

    for reference in &facts.references {
        if reference.kind != ReferenceKind::Call {
            continue;
        }
        let Some(source) = reference.enclosing else {
            // A call at file scope has no caller to attribute the edge to. It is still a use site;
            // it is simply not an edge between two symbols.
            continue;
        };
        let Some(targets) = select(facts, &index, reference, source) else {
            diagnostics.unresolved += 1;
            continue;
        };

        let (tier, candidates) = targets;
        let split = candidates.len();
        if split > 1 {
            diagnostics.ambiguous += 1;
            *diagnostics.ambiguity_by_node.entry(source).or_default() += 1;
        }

        let mut confidence = tier.confidence();
        if index.candidates(&reference.name).len() >= OVER_COMMON_DEFINITIONS {
            confidence *= OVER_COMMON_DEBOOST;
        }
        if reference.name.starts_with('_') {
            confidence *= PRIVATE_DEBOOST;
        }
        // The ambiguity split. Weight is divided evenly rather than assigned to an arbitrary
        // winner, so the ranking absorbs the uncertainty instead of hiding it.
        #[expect(
            clippy::cast_precision_loss,
            reason = "candidate counts are bounded by the definition ceiling, far inside f64"
        )]
        let contribution = confidence / split as f64;

        for target in candidates {
            if target == source {
                // A self-loop is a rank sink in the Google matrix: it inflates the recursive node
                // and steals mass from everything it should be pointing at.
                diagnostics.self_loops_dropped += 1;
                continue;
            }
            let entry = accumulator.entry((source, target)).or_insert((0.0, 0));
            entry.0 += contribution;
            entry.1 += 1;
        }
    }

    build_csr(node_count, accumulator, diagnostics)
}

/// Applies the resolution ladder to one reference.
///
/// Precise tier first where a language makes it sound, then same file, same directory, unique
/// global. If nothing matches at any tier the edge is dropped: no phantom node is ever invented for
/// a name the tree does not define.
fn select(
    facts: &Facts,
    index: &NameIndex,
    reference: &Reference,
    source: NodeId,
) -> Option<(ResolutionTier, Vec<NodeId>)> {
    let candidates = index.candidates(&reference.name);
    if candidates.is_empty() {
        return None;
    }
    let referring_file = facts.file(reference.file)?;

    // Same-language only. A YAML key and a Go function that share a spelling are not the same
    // symbol, and joining them manufactures an edge out of a coincidence.
    let compatible: Vec<NodeId> = candidates
        .iter()
        .copied()
        .filter(|&node| {
            facts
                .definition(node)
                .and_then(|definition| facts.file(definition.file))
                .is_some_and(|file| referring_file.language.compatible_with(file.language))
        })
        .collect();
    if compatible.is_empty() {
        return None;
    }

    if let Some(precise) = select_precise(facts, &compatible, reference, source) {
        return Some((ResolutionTier::Precise, precise));
    }

    let same_file: Vec<NodeId> = compatible
        .iter()
        .copied()
        .filter(|&node| {
            facts
                .definition(node)
                .is_some_and(|definition| definition.file == reference.file)
        })
        .collect();
    if !same_file.is_empty() {
        return Some((ResolutionTier::SameFile, same_file));
    }

    let directory = referring_file.directory();
    let same_directory: Vec<NodeId> = compatible
        .iter()
        .copied()
        .filter(|&node| {
            facts
                .definition(node)
                .and_then(|definition| facts.file(definition.file))
                .is_some_and(|file| file.directory() == directory)
        })
        .collect();
    if !same_directory.is_empty() {
        return Some((ResolutionTier::SameDirectory, same_directory));
    }

    if compatible.len() == 1 {
        return Some((ResolutionTier::UniqueGlobal, compatible));
    }
    // More than one same-language global definition and nothing to choose between them. Splitting
    // across every candidate in the tree would spray weight over unrelated files, so this is a
    // deliberate ambiguity: the reference is recorded as unresolved rather than guessed.
    None
}

/// The precise tiers: qualified forms whose syntax names the target soundly.
///
/// Only shipped where a language's syntax actually supports the rule. Go's qualified calls are
/// deliberately absent: `pkg.Fn()` requires reading the import block to know which package `pkg`
/// binds to, and this crate does not, so a match on the final segment would be a guess wearing a
/// precise label.
fn select_precise(
    facts: &Facts,
    compatible: &[NodeId],
    reference: &Reference,
    source: NodeId,
) -> Option<Vec<NodeId>> {
    let qualifier = reference.qualifier.as_deref()?;
    let source_file = facts.definition(source).map(|definition| definition.file)?;

    // `Self::helper()` names a sibling in the same enclosing type, which is the same file by
    // construction. This is the one qualifier that is sound without cross-file knowledge.
    if qualifier == "Self" || qualifier == "self" {
        let siblings: Vec<NodeId> = compatible
            .iter()
            .copied()
            .filter(|&node| {
                facts
                    .definition(node)
                    .is_some_and(|definition| definition.file == source_file)
            })
            .collect();
        if siblings.len() == 1 {
            return Some(siblings);
        }
        return None;
    }

    // A qualifier that names a type defined in this tree, where exactly one definition of the
    // called name sits in the same file as that type. The pairing is what makes it sound: both the
    // qualifier and the member resolve, and they agree on a file.
    let qualifier_files: Vec<_> = facts
        .definitions
        .iter()
        .filter(|definition| definition.name == qualifier)
        .map(|definition| definition.file)
        .collect();
    if qualifier_files.len() != 1 {
        return None;
    }
    let target_file = qualifier_files[0];
    let members: Vec<NodeId> = compatible
        .iter()
        .copied()
        .filter(|&node| {
            facts
                .definition(node)
                .is_some_and(|definition| definition.file == target_file)
        })
        .collect();
    (members.len() == 1).then_some(members)
}

/// Lays the accumulated pairs out as an in-edge CSR in the two documented passes.
fn build_csr(
    node_count: usize,
    accumulator: HashMap<(NodeId, NodeId), (f64, u32)>,
    diagnostics: ResolutionDiagnostics,
) -> Graph {
    // Sorted so the CSR layout, and therefore every traversal over it, is identical across runs.
    // A HashMap iteration order is not reproducible and must never reach the output.
    let mut edges: Vec<Edge> = accumulator
        .into_iter()
        .map(|((from, to), (confidence_sum, references))| {
            let count = f64::from(references);
            // Mean confidence times the square root of the reference count. The square root is the
            // point: repeated calls should strengthen an edge sublinearly, so a hot loop raises a
            // weight without letting call-site multiplicity alone dominate the ranking.
            let weight = ((confidence_sum / count) * count.sqrt()).min(MAX_EDGE_WEIGHT);
            Edge {
                from,
                to,
                weight,
                references,
            }
        })
        .collect();
    edges.sort_by_key(|edge| (edge.to, edge.from));

    let mut row_offsets = vec![0u32; node_count + 1];
    let mut weighted_out_degree = vec![0.0f64; node_count];

    // Pass one: count in-degree per target and prefix-sum into the row offsets.
    for edge in &edges {
        if let Some(slot) = row_offsets.get_mut(edge.to as usize + 1) {
            *slot += 1;
        }
    }
    for index in 1..row_offsets.len() {
        row_offsets[index] += row_offsets[index - 1];
    }

    // Pass two: scatter each edge into `row = target` at `column = source`. Edges are already
    // sorted by target, so this fills each row contiguously and in ascending source order.
    let mut col_indices = vec![0u32; edges.len()];
    let mut values = vec![0.0f64; edges.len()];
    let mut cursor = row_offsets.clone();
    for edge in &edges {
        let slot = cursor[edge.to as usize] as usize;
        col_indices[slot] = edge.from;
        values[slot] = edge.weight;
        cursor[edge.to as usize] += 1;
        if let Some(degree) = weighted_out_degree.get_mut(edge.from as usize) {
            *degree += edge.weight;
        }
    }

    let dangling = weighted_out_degree
        .iter()
        .map(|&degree| degree <= 0.0)
        .collect();

    Graph {
        row_offsets,
        col_indices,
        values,
        weighted_out_degree,
        dangling,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{IngestLimits, SourceInput, ingest};

    fn graph_of(files: &[(&str, &str)]) -> (Facts, Graph) {
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

    fn has_edge(graph: &Graph, from: NodeId, to: NodeId) -> bool {
        graph.in_edges(to).any(|(source, _)| source == from)
    }

    #[test]
    fn tier_one_prefers_a_definition_in_the_same_file() {
        let (facts, graph) = graph_of(&[
            ("a.rs", "fn target() {}\nfn caller() { target(); }"),
            ("b.rs", "fn target() {}"),
        ]);
        let caller = node(&facts, "caller");
        let local = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("a.rs")
            })
            .expect("local target")
            .node;
        let remote = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("b.rs")
            })
            .expect("remote target")
            .node;
        assert!(has_edge(&graph, caller, local));
        assert!(!has_edge(&graph, caller, remote));
    }

    #[test]
    fn tier_two_prefers_the_same_directory_over_a_distant_definition() {
        let (facts, graph) = graph_of(&[
            ("src/a.rs", "fn caller() { target(); }"),
            ("src/b.rs", "fn target() {}"),
            ("other/c.rs", "fn target() {}"),
        ]);
        let caller = node(&facts, "caller");
        let near = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("src/b.rs")
            })
            .expect("near")
            .node;
        let far = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "target" && facts.path_of(definition.node) == Some("other/c.rs")
            })
            .expect("far")
            .node;
        assert!(has_edge(&graph, caller, near));
        assert!(!has_edge(&graph, caller, far));
    }

    #[test]
    fn tier_three_accepts_a_unique_global_definition() {
        let (facts, graph) = graph_of(&[
            ("src/a.rs", "fn caller() { faraway(); }"),
            ("other/b.rs", "fn faraway() {}"),
        ]);
        assert!(has_edge(
            &graph,
            node(&facts, "caller"),
            node(&facts, "faraway")
        ));
    }

    #[test]
    fn tier_four_drops_the_edge_rather_than_inventing_a_node() {
        let (facts, graph) = graph_of(&[("a.rs", "fn caller() { nowhere_defined(); }")]);
        assert_eq!(graph.edge_count(), 0);
        assert_eq!(graph.diagnostics.unresolved, 1);
        assert_eq!(graph.node_count(), facts.definitions.len());
    }

    #[test]
    fn an_ambiguous_name_splits_weight_rather_than_picking_a_winner() {
        // Two same-directory candidates. Both get an edge, each at half the confidence, and the
        // ambiguity is counted so the caller can see the graph was unsure.
        let (facts, graph) = graph_of(&[
            ("src/a.rs", "fn caller() { target(); }"),
            ("src/b.rs", "fn target() {}"),
            ("src/c.rs", "fn target() {}"),
        ]);
        let caller = node(&facts, "caller");
        let targets: Vec<_> = facts
            .definitions
            .iter()
            .filter(|definition| definition.name == "target")
            .map(|definition| definition.node)
            .collect();
        assert_eq!(targets.len(), 2);
        for target in &targets {
            assert!(has_edge(&graph, caller, *target));
            let weight = graph
                .in_edges(*target)
                .find(|(source, _)| *source == caller)
                .expect("edge")
                .1;
            assert!(
                (weight - ResolutionTier::SameDirectory.confidence() / 2.0).abs() < 1e-9,
                "expected a halved weight, got {weight}"
            );
        }
        assert_eq!(graph.diagnostics.ambiguous, 1);
        assert_eq!(graph.diagnostics.ambiguity_by_node.get(&caller), Some(&1));
    }

    #[test]
    fn a_self_loop_is_dropped_because_it_is_a_rank_sink() {
        let (_, graph) = graph_of(&[("a.rs", "fn recurse(n: u32) { recurse(n - 1); }")]);
        assert_eq!(graph.edge_count(), 0);
        assert_eq!(graph.diagnostics.self_loops_dropped, 1);
    }

    #[test]
    fn repeated_calls_raise_the_weight_sublinearly() {
        let (facts, single) = graph_of(&[("a.rs", "fn t() {}\nfn c() { t(); }")]);
        let (_, quadruple) = graph_of(&[("a.rs", "fn t() {}\nfn c() { t(); t(); t(); t(); }")]);
        let caller = node(&facts, "c");
        let target = node(&facts, "t");
        let weight = |graph: &Graph| {
            graph
                .in_edges(target)
                .find(|(source, _)| *source == caller)
                .expect("edge")
                .1
        };
        let one = weight(&single);
        let four = weight(&quadruple);
        // sqrt(4) == 2, so four calls is twice one call, not four times.
        assert!((four - one * 2.0).abs() < 1e-9, "{one} -> {four}");
    }

    #[test]
    fn an_over_common_name_is_deboosted_where_it_still_resolves() {
        // The deboost only matters when the reference resolves anyway. The caller's own file
        // defines `new`, so tier one answers it; the weight is halved because the name says almost
        // nothing about which `new` was meant.
        let mut files: Vec<(String, String)> = (0..OVER_COMMON_DEFINITIONS)
            .map(|index| (format!("d{index}/m.rs"), "fn new() {}".to_owned()))
            .collect();
        files.push((
            "caller/c.rs".to_owned(),
            "fn new() {}\nfn c() { new(); }".to_owned(),
        ));
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect();
        let (facts, graph) = graph_of(&borrowed);

        let caller = node(&facts, "c");
        let local = facts
            .definitions
            .iter()
            .find(|definition| {
                definition.name == "new" && facts.path_of(definition.node) == Some("caller/c.rs")
            })
            .expect("local new")
            .node;
        let weight = graph
            .in_edges(local)
            .find(|(source, _)| *source == caller)
            .expect("edge")
            .1;
        let undeboosted = ResolutionTier::SameFile.confidence();
        assert!(
            (weight - undeboosted * OVER_COMMON_DEBOOST).abs() < 1e-9,
            "expected the over-common deboost, got {weight} against a base of {undeboosted}"
        );
    }

    #[test]
    fn a_private_name_is_deboosted() {
        let (facts, graph) = graph_of(&[("a.rs", "fn _helper() {}\nfn c() { _helper(); }")]);
        let weight = graph
            .in_edges(node(&facts, "_helper"))
            .find(|(source, _)| *source == node(&facts, "c"))
            .expect("edge")
            .1;
        assert!(
            (weight - ResolutionTier::SameFile.confidence() * PRIVATE_DEBOOST).abs() < 1e-9,
            "expected the private deboost, got {weight}"
        );
    }

    #[test]
    fn many_indistinguishable_globals_are_unresolved_rather_than_sprayed() {
        let files: Vec<(String, String)> = (0..OVER_COMMON_DEFINITIONS)
            .map(|index| (format!("d{index}/m.rs"), "fn new() {}".to_owned()))
            .chain(std::iter::once((
                "caller/c.rs".to_owned(),
                "fn c() { new(); }".to_owned(),
            )))
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect();
        let (_, graph) = graph_of(&borrowed);
        // Nothing distinguishes sixteen same-language globals, so no edge is minted at all.
        // Splitting across every candidate would spray weight over unrelated files and read as
        // sixteen real relationships.
        assert_eq!(graph.diagnostics.unresolved, 1);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn a_config_key_never_answers_a_code_call() {
        let (_, graph) = graph_of(&[
            ("Cargo.toml", "[build]\nname = \"x\"\n"),
            ("a.rs", "fn c() { build(); }"),
        ]);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn csr_rows_are_contiguous_and_offsets_are_monotonic() {
        let (facts, graph) = graph_of(&[(
            "a.rs",
            "fn t() {}\nfn one() { t(); }\nfn two() { t(); }\nfn three() { t(); }",
        )]);
        assert_eq!(graph.row_offsets.len(), facts.definitions.len() + 1);
        for window in graph.row_offsets.windows(2) {
            assert!(window[0] <= window[1], "row offsets must be non-decreasing");
        }
        assert_eq!(
            *graph.row_offsets.last().expect("offsets"),
            u32::try_from(graph.edge_count()).expect("fits")
        );
        let target = node(&facts, "t");
        assert_eq!(graph.in_edges(target).count(), 3);
        let sources: Vec<_> = graph.in_edges(target).map(|(source, _)| source).collect();
        let mut sorted = sources.clone();
        sorted.sort_unstable();
        assert_eq!(sources, sorted, "in-edges must be laid out in source order");
    }

    #[test]
    fn dangling_marks_every_symbol_with_no_outgoing_edge() {
        let (facts, graph) = graph_of(&[("a.rs", "fn leaf() {}\nfn caller() { leaf(); }")]);
        assert!(graph.dangling[node(&facts, "leaf") as usize]);
        assert!(!graph.dangling[node(&facts, "caller") as usize]);
    }

    #[test]
    fn resolution_is_byte_identical_across_repeated_runs() {
        // A HashMap accumulator has no reproducible iteration order, so the CSR is built from a
        // sorted edge list. Without that sort this test fails intermittently, which is the worst
        // possible way for a determinism bug to present.
        let files = [
            ("src/lib.rs", include_str!("lib.rs")),
            ("src/resolve.rs", include_str!("resolve.rs")),
            ("src/extract.rs", include_str!("extract.rs")),
        ];
        let shape = || {
            let (_, graph) = graph_of(&files);
            format!(
                "{:?}|{:?}|{:?}",
                graph.row_offsets, graph.col_indices, graph.values
            )
        };
        let first = shape();
        for _ in 0..4 {
            assert_eq!(first, shape());
        }
    }
}
