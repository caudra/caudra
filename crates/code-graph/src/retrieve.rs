//! Task-shaped retrieval: BM25 over two lanes, fused with the graph ranking.
//!
//! PageRank answers "what is structurally important here". It cannot answer "what should I touch to
//! do *this*", because it does not read the task. This layer does, and it stays a separate stage:
//! the ranking it consumes is unchanged, and a caller can ask for either.
//!
//! Nothing here is a semantic model. Matching is lexical over symbol names, their subtokens, paths,
//! and doc comments. Ask a repository about a concept it does not contain and the best lexical
//! matches still rank, confidently — which is why [`Confidence`] measures ranking separation and
//! says so, rather than implying the head is what you meant.

use std::collections::HashMap;

use crate::{
    model::{Facts, NodeId},
    rank::Ranking,
};

/// BM25 term-frequency saturation.
const BM25_K1: f64 = 1.2;
/// BM25 length normalization.
const BM25_B: f64 = 0.75;
/// Reciprocal-rank fusion constant. Damps the head so one lane cannot dictate the fused order.
const RRF_K: f64 = 60.0;
/// Multiplier for symbols whose path looks like fixtures, generated output, or vendored code.
///
/// Not a filter. Such a symbol can still be the answer, and demoting rather than dropping is what
/// keeps a genuine hit in a `testdata` directory reachable.
const DEPRIORITIZED_PATH_WEIGHT: f64 = 0.35;
/// Multiplier applied when the task text names a symbol's file or module outright.
const MENTION_ANCHOR_BOOST: f64 = 2.5;

/// Which lane the query-shape router chose, and why.
///
/// Reported rather than inferred. A caller comparing two answers needs to know they were produced
/// by different scoring, and a maintainer tuning the router needs to see its decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryShape {
    /// The task names something that looks like a symbol, so exact and near-exact name matches win.
    NameExact,
    /// The task reads as prose, so subtokens, paths, and documentation all contribute.
    Conceptual,
}

impl QueryShape {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NameExact => "name-exact",
            Self::Conceptual => "conceptual",
        }
    }

    /// The reason the router gives for its choice.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NameExact => {
                "the task names an identifier, so name matches are scored ahead of prose"
            }
            Self::Conceptual => {
                "the task reads as prose, so subtokens, paths, and doc comments all score"
            }
        }
    }
}

/// How clearly the ranking separated its head from the rest.
///
/// This measures **separation, not correctness**. A confident answer to a question the repository
/// cannot answer is still confident: the lexical matches that exist still separate. The caller
/// renders this alongside the caveat rather than as a quality claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// One scored candidate.
#[derive(Clone, Copy, Debug)]
pub struct Scored {
    pub node: NodeId,
    pub score: f64,
}

/// A retrieval answer, with everything needed to explain itself.
#[derive(Clone, Debug)]
pub struct Retrieval {
    pub results: Vec<Scored>,
    pub shape: QueryShape,
    pub confidence: Confidence,
    /// Relative gap between the top score and the median of the head, as a percentage.
    pub margin_percent: u32,
    /// Candidates that scored above zero, before the result limit.
    pub total_matched: usize,
}

/// Splits an identifier into lowercase subtokens.
///
/// `buildCatalogEntry`, `build_catalog_entry` and `BuildCatalogEntry` all yield the same three
/// tokens, which is what lets a prose task match a symbol whose spelling it does not know.
///
/// Every non-alphanumeric character separates, so paths and qualified names decompose the same way
/// as identifiers: `src/index/mod.rs` and `Catalog::add` are token sequences, not opaque strings.
/// Without that, a task naming a file could only ever match it by exact substring.
#[must_use]
pub fn subtokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if !character.is_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if character.is_uppercase() && previous_lower && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        previous_lower = character.is_lowercase() || character.is_numeric();
        current.extend(character.to_lowercase());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Whether a path looks like fixtures, generated output, or vendored code.
fn deprioritized(path: &str) -> bool {
    const MARKERS: &[&str] = &[
        "/fixtures/",
        "/fixture/",
        "/testdata/",
        "/test_data/",
        "/vendor/",
        "/third_party/",
        "/generated/",
        "/__snapshots__/",
        "/node_modules/",
    ];
    let padded = format!("/{path}");
    MARKERS.iter().any(|marker| padded.contains(marker))
        || path.ends_with(".pb.go")
        || path.ends_with("_pb2.py")
        || path.ends_with(".min.js")
        || path.ends_with(".generated.ts")
}

/// Chooses a lane from the shape of the task text.
///
/// A task that names an identifier — `snake_case`, `camelCase`, `Type::method`, or a single bare
/// word — is asking about that symbol. Anything longer and prose-shaped is asking about a concept.
#[must_use]
pub fn route(task: &str) -> QueryShape {
    let words: Vec<&str> = task.split_whitespace().collect();
    if words.is_empty() {
        return QueryShape::Conceptual;
    }
    let identifier_shaped = |word: &str| {
        let trimmed = word.trim_matches(|character: char| !character.is_alphanumeric());
        trimmed.len() > 2
            && (trimmed.contains('_')
                || trimmed.contains("::")
                || trimmed.contains('.')
                || (trimmed.chars().any(char::is_uppercase)
                    && trimmed.chars().any(char::is_lowercase)
                    && !trimmed.chars().next().is_some_and(char::is_uppercase)))
    };
    if words.len() == 1 && words[0].len() > 2 {
        return QueryShape::NameExact;
    }
    if words.len() <= 3 && words.iter().any(|word| identifier_shaped(word)) {
        return QueryShape::NameExact;
    }
    QueryShape::Conceptual
}

/// A BM25 index over one lane of the corpus.
struct Bm25 {
    /// Document frequency per term.
    document_frequency: HashMap<String, usize>,
    /// Per-document term counts, parallel to the definition table.
    documents: Vec<HashMap<String, usize>>,
    lengths: Vec<usize>,
    average_length: f64,
    document_count: usize,
}

impl Bm25 {
    fn build(documents: Vec<Vec<String>>) -> Self {
        let document_count = documents.len();
        let mut document_frequency: HashMap<String, usize> = HashMap::new();
        let mut counted = Vec::with_capacity(document_count);
        let mut lengths = Vec::with_capacity(document_count);
        let mut total_length = 0usize;

        for terms in documents {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for term in &terms {
                *counts.entry(term.clone()).or_default() += 1;
            }
            for term in counts.keys() {
                *document_frequency.entry(term.clone()).or_default() += 1;
            }
            lengths.push(terms.len());
            total_length += terms.len();
            counted.push(counts);
        }

        #[expect(
            clippy::cast_precision_loss,
            reason = "corpus sizes are bounded by the ingest ceiling, far inside f64"
        )]
        let average_length = if document_count == 0 {
            0.0
        } else {
            total_length as f64 / document_count as f64
        };

        Self {
            document_frequency,
            documents: counted,
            lengths,
            average_length,
            document_count,
        }
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "corpus and term counts are bounded by the ingest ceiling, far inside f64"
    )]
    fn score(&self, document: usize, query: &[String]) -> f64 {
        if self.average_length <= 0.0 {
            return 0.0;
        }
        let Some(counts) = self.documents.get(document) else {
            return 0.0;
        };
        let length = self.lengths[document] as f64;
        let mut score = 0.0;
        for term in query {
            let Some(&frequency) = counts.get(term) else {
                continue;
            };
            let document_frequency = *self.document_frequency.get(term).unwrap_or(&0) as f64;
            let total = self.document_count as f64;
            // Robertson/Sparck-Jones IDF with the +0.5 smoothing, so a term present in most
            // documents contributes near zero rather than a negative score.
            let idf = ((total - document_frequency + 0.5) / (document_frequency + 0.5) + 1.0).ln();
            let frequency = frequency as f64;
            let denominator =
                frequency + BM25_K1 * BM25_B.mul_add(length / self.average_length, 1.0 - BM25_B);
            score += idf * (frequency * (BM25_K1 + 1.0)) / denominator;
        }
        score
    }
}

/// Ranks definitions against a task description.
///
/// Fuses three signals with reciprocal-rank fusion: the name lane, the prose lane, and the graph
/// ranking. RRF is used rather than a weighted score sum because the three produce incomparable
/// magnitudes, and normalizing them against each other would invent a calibration none of them has.
#[must_use]
pub fn retrieve(facts: &Facts, ranking: &Ranking, task: &str, limit: usize) -> Retrieval {
    let shape = route(task);
    let query: Vec<String> = subtokens(task);

    let mut name_lane = Vec::with_capacity(facts.definitions.len());
    let mut prose_lane = Vec::with_capacity(facts.definitions.len());
    for definition in &facts.definitions {
        let path = facts
            .file(definition.file)
            .map_or("", |file| file.path.as_str());
        name_lane.push(subtokens(&definition.name));
        let mut prose = subtokens(&definition.name);
        prose.extend(subtokens(path));
        if let Some(documentation) = &definition.documentation {
            prose.extend(subtokens(documentation));
        }
        prose.push(definition.kind.name().to_owned());
        prose_lane.push(prose);
    }

    let names = Bm25::build(name_lane);
    let prose = Bm25::build(prose_lane);

    let lowered = task.to_lowercase();
    let mut lexical: Vec<Scored> = Vec::with_capacity(facts.definitions.len());
    for (index, definition) in facts.definitions.iter().enumerate() {
        let name_score = names.score(index, &query);
        let prose_score = prose.score(index, &query);
        let mut score = match shape {
            // The router does not switch lanes outright; it reweights them. A name-shaped task
            // still benefits from a matching doc comment, just less.
            QueryShape::NameExact => prose_score.mul_add(0.25, name_score * 2.0),
            QueryShape::Conceptual => name_score.mul_add(0.5, prose_score),
        };
        if score > 0.0 {
            let path = facts
                .file(definition.file)
                .map_or("", |file| file.path.as_str());
            // Query-mention anchoring: the task naming a file or module outright is a much stronger
            // signal than any term overlap, because it is a caller pointing at a location.
            if mentions(&lowered, path) || lowered.contains(&definition.name.to_lowercase()) {
                score *= MENTION_ANCHOR_BOOST;
            }
            if deprioritized(path) {
                score *= DEPRIORITIZED_PATH_WEIGHT;
            }
        }
        lexical.push(Scored {
            node: definition.node,
            score,
        });
    }

    let total_matched = lexical.iter().filter(|scored| scored.score > 0.0).count();
    let fused = fuse(&lexical, ranking, limit);
    let (confidence, margin_percent) = separation(&fused);

    Retrieval {
        results: fused,
        shape,
        confidence,
        margin_percent,
        total_matched,
    }
}

/// Whether the task text names this path, its file stem, or one of its directories.
fn mentions(lowered_task: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let lowered = path.to_lowercase();
    if lowered_task.contains(&lowered) {
        return true;
    }
    let stem = lowered
        .rsplit('/')
        .next()
        .and_then(|name| name.split('.').next())
        .unwrap_or_default();
    // Two characters is not a mention, it is a coincidence.
    stem.len() > 3
        && lowered_task
            .split_whitespace()
            .any(|word| word.trim_matches(|character: char| !character.is_alphanumeric()) == stem)
}

/// Competition ranks for `candidates` under `score`, positionally aligned with the input.
///
/// Equal scores share the rank of the first member of their run, so a lane with no preference
/// between two candidates contributes the same term for both.
fn tied_ranks<F: Fn(&Scored) -> f64>(candidates: &[&Scored], score: F) -> Vec<usize> {
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&left, &right| {
        score(candidates[right])
            .partial_cmp(&score(candidates[left]))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut ranks = vec![0_usize; candidates.len()];
    let mut previous: Option<f64> = None;
    let mut rank = 0_usize;
    for (position, &index) in order.iter().enumerate() {
        let current = score(candidates[index]);
        if previous.is_none_or(|last| last != current) {
            rank = position;
            previous = Some(current);
        }
        ranks[index] = rank;
    }
    ranks
}

/// Reciprocal-rank fusion of the lexical score and the graph ranking.
///
/// Only symbols with a non-zero lexical score participate. Fusing over the whole corpus would let
/// PageRank alone answer a question it never read, which is how a task-shaped query ends up
/// returning a repository's most-called utility regardless of what was asked.
///
/// Both lanes are ranked over that same candidate set. Using each symbol's position in the global
/// rank vector instead would make the graph lane a no-op for every candidate outside the repository
/// head, since `1/(k + 5000)` is indistinguishable from `1/(k + 5001)`: importance among the
/// symbols that actually matched is the question, not importance in the repository.
///
/// Equal scores within a lane share a rank. Breaking a lane's internal ties by node id before
/// fusion would promote an arbitrary ordering into a vote: two symbols a lane genuinely cannot
/// distinguish would arrive with different ranks, and that manufactured difference would outweigh
/// the other lane's real one. A lane that has no opinion must cast no vote.
fn fuse(lexical: &[Scored], ranking: &Ranking, limit: usize) -> Vec<Scored> {
    let mut candidates: Vec<&Scored> = lexical.iter().filter(|scored| scored.score > 0.0).collect();
    candidates.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.node.cmp(&right.node))
    });

    let lexical_rank = tied_ranks(&candidates, |scored| scored.score);
    let graph_rank = tied_ranks(&candidates, |scored| {
        ranking
            .scores
            .get(scored.node as usize)
            .copied()
            .unwrap_or(0.0)
    });

    #[expect(
        clippy::cast_precision_loss,
        reason = "ranks are bounded by the definition ceiling, far inside f64"
    )]
    let contribution = |rank: usize| 1.0 / (RRF_K + rank as f64 + 1.0);
    let mut fused: Vec<Scored> = candidates
        .iter()
        .enumerate()
        .map(|(position, scored)| Scored {
            node: scored.node,
            score: contribution(lexical_rank[position]) + contribution(graph_rank[position]),
        })
        .collect();

    fused.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.node.cmp(&right.node))
    });
    fused.truncate(limit);
    fused
}

/// Derives confidence from how far the top score sits above the head's median.
fn separation(results: &[Scored]) -> (Confidence, u32) {
    if results.len() < 2 {
        // One result cannot be separated from anything. Reporting high confidence for a single
        // match would be a claim about correctness, which this measure does not make.
        return (Confidence::Low, 0);
    }
    let head = &results[..results.len().min(10)];
    let top = head[0].score;
    let median = head[head.len() / 2].score;
    if top <= 0.0 {
        return (Confidence::Low, 0);
    }
    let margin = (top - median) / top;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "margin is a ratio in [0, 1] scaled to a percentage"
    )]
    let percent = (margin * 100.0).round().clamp(0.0, 100.0) as u32;
    let confidence = if margin >= 0.15 {
        Confidence::High
    } else if margin >= 0.05 {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    (confidence, percent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ingest::{IngestLimits, SourceInput, ingest},
        rank::{Teleport, pagerank},
        resolve::resolve,
    };

    fn corpus(files: &[(&str, &str)]) -> (Facts, Ranking) {
        let inputs = files
            .iter()
            .map(|(path, source)| SourceInput {
                path: (*path).to_owned(),
                source: (*source).to_owned(),
            })
            .collect();
        let ingested = ingest(inputs, IngestLimits::default());
        let graph = resolve(&ingested.facts);
        let ranking = pagerank(&graph, &Teleport::Uniform);
        (ingested.facts, ranking)
    }

    fn names(facts: &Facts, retrieval: &Retrieval) -> Vec<String> {
        retrieval
            .results
            .iter()
            .filter_map(|scored| facts.definition(scored.node))
            .map(|definition| definition.name.clone())
            .collect()
    }

    #[test]
    fn subtokens_split_every_common_identifier_convention() {
        assert_eq!(
            subtokens("buildCatalogEntry"),
            ["build", "catalog", "entry"]
        );
        assert_eq!(
            subtokens("build_catalog_entry"),
            ["build", "catalog", "entry"]
        );
        assert_eq!(subtokens("src/index/mod.rs"), ["src", "index", "mod", "rs"]);
        assert_eq!(subtokens("HTTPServer"), ["httpserver"]);
        assert!(subtokens("").is_empty());
    }

    #[test]
    fn the_router_reports_its_choice_and_its_reason() {
        assert_eq!(route("build_catalog"), QueryShape::NameExact);
        assert_eq!(route("Catalog::add"), QueryShape::NameExact);
        assert_eq!(route("normalize"), QueryShape::NameExact);
        assert_eq!(
            route("where do we invalidate the cache after a write"),
            QueryShape::Conceptual
        );
        assert!(QueryShape::NameExact.reason().contains("identifier"));
        assert!(QueryShape::Conceptual.reason().contains("prose"));
    }

    #[test]
    fn a_named_symbol_ranks_first_for_its_own_name() {
        let (facts, ranking) = corpus(&[(
            "src/catalog.rs",
            "fn normalize_sku(s: &str) {}\nfn unrelated_thing() {}\nfn another_one() {}",
        )]);
        let retrieval = retrieve(&facts, &ranking, "normalize_sku", 5);
        assert_eq!(
            names(&facts, &retrieval).first().map(String::as_str),
            Some("normalize_sku")
        );
    }

    #[test]
    fn a_prose_task_finds_a_symbol_whose_spelling_it_did_not_use() {
        let (facts, ranking) = corpus(&[(
            "src/cache.rs",
            "fn invalidateCacheEntry() {}\nfn renderTemplate() {}\nfn parseHeader() {}",
        )]);
        let retrieval = retrieve(&facts, &ranking, "how do we invalidate a cache entry", 5);
        assert_eq!(retrieval.shape, QueryShape::Conceptual);
        assert_eq!(
            names(&facts, &retrieval).first().map(String::as_str),
            Some("invalidateCacheEntry")
        );
    }

    #[test]
    fn a_fixture_path_is_demoted_but_still_reachable() {
        let (facts, ranking) = corpus(&[
            ("src/catalog.rs", "fn normalize() {}"),
            ("tests/fixtures/catalog.rs", "fn normalize() {}"),
        ]);
        let retrieval = retrieve(&facts, &ranking, "normalize", 5);
        let ordered = names(&facts, &retrieval);
        assert_eq!(ordered.len(), 2, "the fixture must stay reachable");
        let paths: Vec<_> = retrieval
            .results
            .iter()
            .filter_map(|scored| facts.path_of(scored.node))
            .collect();
        assert_eq!(paths[0], "src/catalog.rs", "production should lead");
    }

    #[test]
    fn naming_a_file_in_the_task_anchors_its_symbols() {
        let (facts, ranking) = corpus(&[
            ("src/alpha.rs", "fn handler() {}"),
            ("src/beta.rs", "fn handler() {}"),
        ]);
        let retrieval = retrieve(&facts, &ranking, "the handler in src/beta.rs", 5);
        let paths: Vec<_> = retrieval
            .results
            .iter()
            .filter_map(|scored| facts.path_of(scored.node))
            .collect();
        assert_eq!(paths.first().copied(), Some("src/beta.rs"));
    }

    #[test]
    fn the_graph_lane_still_breaks_a_lexical_tie() {
        // Counterpart to the anchoring test. Weighting the lexical lane above the graph lane must
        // not reduce the graph lane to decoration: two candidates the lexical lane cannot tell
        // apart are exactly the case importance is supposed to decide, and the winner must be the
        // called symbol rather than the lower node id.
        // The call sits inside beta so tier one resolves it there outright. A call from a third
        // file would be ambiguous between the two definitions, and the resolver's 1/k split would
        // leave the ranking flat on purpose, testing nothing.
        let (facts, ranking) = corpus(&[
            ("src/alpha.rs", "fn handler() {}"),
            (
                "src/beta.rs",
                "fn handler() {}\nfn drive() { handler(); handler(); handler(); }",
            ),
        ]);
        let beta = facts
            .definitions
            .iter()
            .find(|definition| facts.path_of(definition.node) == Some("src/beta.rs"))
            .expect("beta defines handler");
        let alpha = facts
            .definitions
            .iter()
            .find(|definition| facts.path_of(definition.node) == Some("src/alpha.rs"))
            .expect("alpha defines handler");
        assert!(
            ranking.scores[beta.node as usize] > ranking.scores[alpha.node as usize]
                || ranking.scores[alpha.node as usize] > ranking.scores[beta.node as usize],
            "the fixture must produce a graph that actually separates the two definitions"
        );

        let retrieval = retrieve(&facts, &ranking, "handler", 5);
        let leader = retrieval.results.first().expect("a match");
        let expected = if ranking.scores[beta.node as usize] > ranking.scores[alpha.node as usize] {
            beta.node
        } else {
            alpha.node
        };
        assert_eq!(
            leader.node, expected,
            "importance among the matched symbols should decide, not the node id"
        );
    }

    #[test]
    fn nothing_matching_returns_an_empty_result_not_a_ranking_of_everything() {
        // Fusing over the whole corpus would let PageRank answer a question it never read, and the
        // caller would receive the repository's busiest utility regardless of what was asked.
        let (facts, ranking) = corpus(&[("src/a.rs", "fn alpha() {}\nfn beta() {}")]);
        let retrieval = retrieve(&facts, &ranking, "zzzznonexistentterm", 5);
        assert!(retrieval.results.is_empty());
        assert_eq!(retrieval.total_matched, 0);
        assert_eq!(retrieval.confidence, Confidence::Low);
    }

    #[test]
    fn confidence_reports_separation_and_a_flat_ranking_reads_low() {
        let flat = [
            Scored {
                node: 0,
                score: 1.0,
            },
            Scored {
                node: 1,
                score: 1.0,
            },
            Scored {
                node: 2,
                score: 1.0,
            },
        ];
        let (confidence, margin) = separation(&flat);
        assert_eq!(confidence, Confidence::Low);
        assert_eq!(margin, 0);

        let separated = [
            Scored {
                node: 0,
                score: 1.0,
            },
            Scored {
                node: 1,
                score: 0.5,
            },
            Scored {
                node: 2,
                score: 0.4,
            },
        ];
        let (confidence, margin) = separation(&separated);
        assert_eq!(confidence, Confidence::High);
        assert_eq!(margin, 50);
    }

    #[test]
    fn a_single_result_is_never_reported_as_high_confidence() {
        let (confidence, margin) = separation(&[Scored {
            node: 0,
            score: 9.0,
        }]);
        assert_eq!(confidence, Confidence::Low);
        assert_eq!(margin, 0);
    }

    #[test]
    fn retrieval_is_deterministic_across_repeated_runs() {
        let files = [
            ("src/retrieve.rs", include_str!("retrieve.rs")),
            ("src/rank.rs", include_str!("rank.rs")),
            ("src/resolve.rs", include_str!("resolve.rs")),
        ];
        let run = || {
            let (facts, ranking) = corpus(&files);
            let retrieval = retrieve(&facts, &ranking, "rank the graph deterministically", 20);
            retrieval
                .results
                .iter()
                .map(|scored| (scored.node, scored.score.to_bits()))
                .collect::<Vec<_>>()
        };
        let first = run();
        for _ in 0..3 {
            assert_eq!(first, run());
        }
    }

    #[test]
    fn results_respect_the_limit() {
        let (facts, ranking) = corpus(&[(
            "src/a.rs",
            "fn handler_one() {}\nfn handler_two() {}\nfn handler_three() {}\nfn handler_four() {}",
        )]);
        let retrieval = retrieve(&facts, &ranking, "handler", 2);
        assert_eq!(retrieval.results.len(), 2);
        assert_eq!(retrieval.total_matched, 4, "total is the pre-limit count");
    }
}
