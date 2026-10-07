//! Building one graph and answering the five questions against it.
//!
//! Each tool call builds a graph, answers, and drops it. The group is stateless between calls
//! except for the extraction cache, which is a cost decision that cannot change an answer: the
//! engine's warm==cold gate is what makes that claim checkable rather than aspirational.

use std::collections::{BTreeSet, HashMap};

use workcell_code_graph::{
    Facts, FactsCache, Graph, NodeId, Ranking, SymbolKind, Teleport, ingest_cached, pagerank,
    reaching_hops, resolve, retrieve,
};

use crate::{
    crawl::Crawled,
    limits::CodeGraphLimits,
    progress::{GraphPhase, PhaseNotifier},
    types::{Direction, GraphSummary, RankedSymbol, ReachedSymbol, SelectorRefusal, SymbolRef},
};

/// Bytes per token used for the `estimatedTokens` field.
///
/// Calibrated, not exact, and reported as such everywhere it surfaces. Source text tokenizes more
/// densely than prose because identifiers, punctuation, and indentation fragment; four bytes per
/// token is the conservative end of the range measured across the languages this indexes, so the
/// estimate errs toward overstating cost. A caller budgeting against it will under-fill rather than
/// overrun, which is the safe direction for a context window.
const BYTES_PER_TOKEN: usize = 4;

/// A calibrated token estimate for a byte count.
#[must_use]
pub fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// Levenshtein distance, bounded so a long pair costs no more than a short one.
///
/// Used only to offer did-you-mean candidates. Nothing branches on it, so an approximation that
/// stops early is preferable to an exact answer over a pathological input.
fn edit_distance(left: &str, right: &str, ceiling: usize) -> usize {
    let left: Vec<char> = left.chars().take(64).collect();
    let right: Vec<char> = right.chars().take(64).collect();
    if left.len().abs_diff(right.len()) > ceiling {
        return ceiling + 1;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0_usize; right.len() + 1];
    for (i, left_char) in left.iter().enumerate() {
        current[0] = i + 1;
        let mut row_best = current[0];
        for (j, right_char) in right.iter().enumerate() {
            let cost = usize::from(left_char != right_char);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            row_best = row_best.min(current[j + 1]);
        }
        if row_best > ceiling {
            return ceiling + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// A built graph over one crawl.
pub struct CodeGraph {
    pub facts: Facts,
    pub graph: Graph,
    pub ranking: Ranking,
    out_adjacency: Vec<Vec<NodeId>>,
    pub summary: GraphSummary,
}

impl CodeGraph {
    /// Ingests, resolves, and ranks a crawl.
    pub fn build(
        crawled: Crawled,
        limits: &CodeGraphLimits,
        cache: &mut FactsCache,
        notify: Option<&PhaseNotifier>,
    ) -> Self {
        let files = crawled.inputs.len();
        if let Some(notify) = notify {
            notify(GraphPhase::Parse, files);
        }
        let mut truncated_by: Vec<String> = crawled
            .truncated
            .map(|reason| vec![reason.name().to_owned()])
            .unwrap_or_default();
        let skipped_by_crawl = crawled.skipped.len();
        if !crawled.ignore_complete {
            // Not a truncated scan: the exclusions are the subset, so the map is a superset of the
            // intended one. Named separately for that reason.
            truncated_by.push("gitignore_rules".to_owned());
        }
        let files_ignored = crawled.files_ignored;
        let pruned_repositories = crawled.pruned_repositories;

        let ingested = ingest_cached(crawled.inputs, limits.ingest, cache);
        if ingested.truncation.files {
            truncated_by.push("ingest_files".to_owned());
        }
        if ingested.truncation.definitions {
            truncated_by.push("definitions".to_owned());
        }
        if ingested.truncation.references {
            truncated_by.push("references".to_owned());
        }

        let facts = ingested.facts;
        if let Some(notify) = notify {
            notify(GraphPhase::Rank, files);
        }
        let graph = resolve(&facts);
        let ranking = pagerank(&graph, &Teleport::Uniform);
        if !ranking.converged {
            truncated_by.push("pagerank_iterations".to_owned());
        }
        let out_adjacency = graph.out_adjacency();

        let resolved = facts
            .references
            .len()
            .saturating_sub(graph.diagnostics.unresolved);
        let summary = GraphSummary {
            files_indexed: facts.files.len(),
            files_skipped: skipped_by_crawl + facts.skipped.len(),
            symbols: facts.definitions.len(),
            resolved_references: resolved,
            unresolved_references: graph.diagnostics.unresolved,
            ambiguous_references: graph.diagnostics.ambiguous,
            edges: graph.edge_count(),
            pr_iterations: ranking.iterations,
            pr_converged: ranking.converged,
            truncated_by,
            scan_complete: crawled.scan_complete,
            files_ignored,
            pruned_repositories,
        };

        Self {
            facts,
            graph,
            ranking,
            out_adjacency,
            summary,
        }
    }

    /// In-degree of one symbol: how many distinct symbols reference it. A floor.
    #[must_use]
    pub fn callers_of(&self, node: NodeId) -> Vec<NodeId> {
        let mut callers: Vec<NodeId> = self
            .graph
            .in_edges(node)
            .map(|(source, _)| source)
            .collect();
        callers.sort_unstable();
        callers.dedup();
        callers
    }

    /// Out-degree of one symbol: how many distinct symbols it references. A floor.
    #[must_use]
    pub fn callees_of(&self, node: NodeId) -> &[NodeId] {
        self.out_adjacency
            .get(node as usize)
            .map_or(&[], Vec::as_slice)
    }

    /// Renders one symbol with its rank and both degrees.
    #[must_use]
    pub fn ranked(&self, node: NodeId) -> Option<RankedSymbol> {
        let definition = self.facts.definition(node)?;
        Some(RankedSymbol {
            name: definition.name.clone(),
            kind: kind_name(definition.kind).to_owned(),
            path: self.facts.path_of(node).unwrap_or_default().to_owned(),
            line_start: definition.lines.start,
            line_end: definition.lines.end,
            rank: self
                .ranking
                .scores
                .get(node as usize)
                .copied()
                .unwrap_or(0.0),
            callers: self.callers_of(node).len(),
            calls: self.callees_of(node).len(),
            test_scope: definition.test_scope,
        })
    }

    /// Renders one symbol without a rank.
    #[must_use]
    pub fn symbol_ref(&self, node: NodeId) -> Option<SymbolRef> {
        let definition = self.facts.definition(node)?;
        Some(SymbolRef {
            name: definition.name.clone(),
            kind: kind_name(definition.kind).to_owned(),
            path: self.facts.path_of(node).unwrap_or_default().to_owned(),
            line_start: definition.lines.start,
            line_end: definition.lines.end,
        })
    }

    /// Symbols ranked highest first, as node ids.
    #[must_use]
    pub fn ordered(&self) -> Vec<NodeId> {
        self.ranking.ordered()
    }

    /// Resolves a selector to every definition it names.
    ///
    /// A selector may be a bare name or `path::name`, where `path` is any suffix of the defining
    /// file's path. Matching every definition rather than picking one is deliberate: two symbols
    /// can legitimately share a name, and choosing between them silently would answer a question
    /// about one while the caller was asking about the other.
    pub fn select(
        &self,
        selector: &str,
        limits: &CodeGraphLimits,
    ) -> Result<Vec<NodeId>, SelectorRefusal> {
        let (path_hint, name) = split_selector(selector);
        let matched: Vec<NodeId> = self
            .facts
            .definitions
            .iter()
            .filter(|definition| definition.name == name)
            .filter(|definition| {
                path_hint.is_none_or(|hint| {
                    self.facts
                        .path_of(definition.node)
                        .is_some_and(|path| path_matches(path, hint))
                })
            })
            .map(|definition| definition.node)
            .collect();

        if !matched.is_empty() {
            return Ok(matched);
        }
        Err(SelectorRefusal::new(
            selector.to_owned(),
            self.suggest(name, limits.max_suggestions),
            self.facts.definitions.len(),
        ))
    }

    /// Known names nearest `name` by edit distance, deduplicated and ordered.
    fn suggest(&self, name: &str, limit: usize) -> Vec<String> {
        // A third of the length, so a short name does not match everything and a long one still
        // tolerates a typo or two.
        let ceiling = (name.chars().count() / 3).clamp(1, 5);
        let mut scored: Vec<(usize, &str)> = Vec::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for definition in &self.facts.definitions {
            if !seen.insert(definition.name.as_str()) {
                continue;
            }
            let distance = edit_distance(name, &definition.name, ceiling);
            if distance <= ceiling {
                scored.push((distance, definition.name.as_str()));
            }
        }
        scored.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, name)| name.to_owned())
            .collect()
    }

    /// Callers or callees of every node in `seeds`, ranked.
    #[must_use]
    pub fn references(&self, seeds: &[NodeId], direction: Direction) -> Vec<RankedSymbol> {
        let mut nodes: Vec<NodeId> = match direction {
            Direction::Callers => seeds
                .iter()
                .flat_map(|&seed| self.callers_of(seed))
                .collect(),
            Direction::Callees => seeds
                .iter()
                .flat_map(|&seed| self.callees_of(seed).iter().copied())
                .collect(),
        };
        nodes.sort_unstable();
        nodes.dedup();
        let seeds: BTreeSet<NodeId> = seeds.iter().copied().collect();
        let mut rows: Vec<RankedSymbol> = nodes
            .into_iter()
            .filter(|node| !seeds.contains(node))
            .filter_map(|node| self.ranked(node))
            .collect();
        sort_by_rank(&mut rows);
        rows
    }

    /// Symbols reaching `seeds` within `depth` hops, nearest first.
    #[must_use]
    pub fn impact(&self, seeds: &[NodeId], depth: usize, limit: usize) -> Vec<ReachedSymbol> {
        reaching_hops(&self.graph, seeds, depth, limit)
            .into_iter()
            .filter_map(|(node, hops)| {
                let definition = self.facts.definition(node)?;
                Some(ReachedSymbol {
                    symbol: self.symbol_ref(node)?,
                    hops,
                    test_scope: definition.test_scope,
                })
            })
            .collect()
    }

    /// Task-shaped retrieval over the built graph.
    #[must_use]
    pub fn retrieve(&self, task: &str, limit: usize) -> retrieve::Retrieval {
        retrieve::retrieve(&self.facts, &self.ranking, task, limit)
    }
}

/// Orders by descending rank, then by path and line so equal ranks do not churn between runs.
pub fn sort_by_rank(rows: &mut [RankedSymbol]) {
    rows.sort_by(|left, right| {
        right
            .rank
            .partial_cmp(&left.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line_start.cmp(&right.line_start))
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Splits `path::name` or `path:name` into its parts.
///
/// The last `::` or `:` wins, so a qualifier containing either still resolves.
fn split_selector(selector: &str) -> (Option<&str>, &str) {
    if let Some(index) = selector.rfind("::") {
        let (path, name) = selector.split_at(index);
        if !path.is_empty() && name.len() > 2 {
            return (Some(path), &name[2..]);
        }
    }
    if let Some(index) = selector.rfind(':') {
        let (path, name) = selector.split_at(index);
        if !path.is_empty() && name.len() > 1 {
            return (Some(path), &name[1..]);
        }
    }
    (None, selector)
}

/// Whether `hint` names `path`: an exact match, a path suffix, or a file stem.
fn path_matches(path: &str, hint: &str) -> bool {
    if path == hint || path.ends_with(&format!("/{hint}")) {
        return true;
    }
    let stem = path
        .rsplit('/')
        .next()
        .and_then(|name| name.split('.').next())
        .unwrap_or_default();
    stem == hint
}

/// A stable lowercase name for a symbol kind.
#[must_use]
pub fn kind_name(kind: SymbolKind) -> &'static str {
    kind.name()
}

/// Counts how many nodes carry each name, for ambiguity disclosure.
#[must_use]
pub fn name_counts(facts: &Facts) -> HashMap<&str, usize> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for definition in &facts.definitions {
        *counts.entry(definition.name.as_str()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_may_name_a_path_or_a_file_stem() {
        assert_eq!(split_selector("alpha"), (None, "alpha"));
        assert_eq!(
            split_selector("src/a.rs::alpha"),
            (Some("src/a.rs"), "alpha")
        );
        assert_eq!(split_selector("a:alpha"), (Some("a"), "alpha"));
        assert!(path_matches("src/deep/a.rs", "src/deep/a.rs"));
        assert!(path_matches("src/deep/a.rs", "deep/a.rs"));
        assert!(path_matches("src/deep/a.rs", "a"));
        assert!(!path_matches("src/deep/a.rs", "b"));
    }

    #[test]
    fn edit_distance_stops_early_rather_than_scoring_a_distant_pair() {
        assert_eq!(edit_distance("alpha", "alpha", 3), 0);
        assert_eq!(edit_distance("alpha", "alpho", 3), 1);
        assert!(
            edit_distance("alpha", "completely_different_name", 3) > 3,
            "a distant pair must not be offered as a suggestion"
        );
    }

    #[test]
    fn the_token_estimate_is_labelled_conservative_and_behaves_that_way() {
        // Overstating cost makes a caller under-fill a context window; understating it makes them
        // overrun one. The estimate must err in the first direction.
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
    }
}
