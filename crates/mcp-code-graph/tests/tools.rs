//! End-to-end behaviour of the five tools against a real tree on disk.

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_code_graph::{
    CodeContextInput, CodeExpandInput, CodeGraphToolGroup, CodeImpactInput, CodeMapInput,
    CodeRefsInput, Direction, GraphPhase, GraphProgress, GraphProgressSink, ModelText,
};

/// A tree with a clear importance gradient: `normalize` is called from three places, `orphan` from
/// none, and a test reaches the middle of the chain.
const TREE: &[(&str, &str)] = &[
    (
        "src/normalize.rs",
        "/// Normalizes a SKU.\npub fn normalize_sku(input: &str) -> String { input.trim().to_owned() }\n",
    ),
    (
        "src/catalog.rs",
        "use crate::normalize::normalize_sku;\npub fn add_item(sku: &str) { normalize_sku(sku); }\npub fn update_item(sku: &str) { normalize_sku(sku); }\n",
    ),
    (
        "src/import.rs",
        "use crate::normalize::normalize_sku;\npub fn import_row(sku: &str) { normalize_sku(sku); }\npub fn orphan_helper() {}\n",
    ),
    (
        "src/cache.rs",
        "pub fn invalidate_cache_entry(key: &str) { let _ = key; }\npub fn render_template() {}\n",
    ),
    (
        "tests/catalog_test.rs",
        "#[test]\nfn covers_add_item() { crate::catalog::add_item(\"x\"); }\n",
    ),
];

async fn group() -> (TempDir, CodeGraphToolGroup) {
    let directory = TempDir::new().expect("temp dir");
    for (path, contents) in TREE {
        let full = directory.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(full, contents).expect("write");
    }
    let group = CodeGraphToolGroup::new(directory.path(), None)
        .await
        .expect("group");
    (directory, group)
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

#[tokio::test]
async fn code_map_ranks_the_most_referenced_symbol_first() {
    let (_directory, group) = group().await;
    let output = group
        .code_map(CodeMapInput::default(), None, &token())
        .await
        .expect("map");

    assert!(output.graph.files_indexed >= 5);
    assert!(output.counts_floor, "every count here is a floor");
    assert!(output.graph.pr_converged);
    assert_eq!(
        output.symbols.first().map(|symbol| symbol.name.as_str()),
        Some("normalize_sku"),
        "the symbol three others call should lead: {:?}",
        output
            .symbols
            .iter()
            .map(|s| (&s.name, s.callers))
            .collect::<Vec<_>>()
    );
    let leader = &output.symbols[0];
    assert_eq!(leader.callers, 3, "three distinct callers");
    assert_eq!(leader.path, "src/normalize.rs");
}

#[tokio::test]
async fn code_map_paths_are_root_relative_whether_or_not_the_map_is_scoped() {
    let (_directory, group) = group().await;
    let whole = group
        .code_map(CodeMapInput::default(), None, &token())
        .await
        .expect("map");
    let scoped = group
        .code_map(
            CodeMapInput {
                path: Some("src".to_owned()),
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("scoped map");

    let find = |symbols: &[workcell_mcp_code_graph::RankedSymbol]| {
        symbols
            .iter()
            .find(|symbol| symbol.name == "normalize_sku")
            .map(|symbol| symbol.path.clone())
    };
    assert_eq!(
        find(&whole.symbols),
        find(&scoped.symbols),
        "a path from a scoped map must be usable against the whole map"
    );
    assert_eq!(find(&scoped.symbols).as_deref(), Some("src/normalize.rs"));
    assert!(
        scoped.graph.files_indexed < whole.graph.files_indexed,
        "scoping must actually narrow the crawl"
    );
}

#[tokio::test]
async fn code_context_finds_a_symbol_the_task_did_not_spell() {
    let (_directory, group) = group().await;
    let output = group
        .code_context(
            CodeContextInput {
                task: "where do we invalidate a cache entry".to_owned(),
                path: None,
                limit: Some(5),
            },
            None,
            &token(),
        )
        .await
        .expect("context");

    assert_eq!(output.shape, "conceptual");
    assert_eq!(
        output.results.first().map(|symbol| symbol.name.as_str()),
        Some("invalidate_cache_entry")
    );
    assert!(output.estimated_tokens > 0);
}

#[tokio::test]
async fn code_context_returns_nothing_for_an_unrelated_task() {
    // The alternative is returning the repository's busiest symbol, which answers a question that
    // was not asked and reads exactly like a real answer.
    let (_directory, group) = group().await;
    let output = group
        .code_context(
            CodeContextInput {
                task: "quantum chromodynamics lattice solver".to_owned(),
                path: None,
                limit: Some(5),
            },
            None,
            &token(),
        )
        .await
        .expect("context");
    assert!(output.results.is_empty(), "{:?}", output.results);
    assert_eq!(output.total_matched, 0);
    assert!(
        output
            .model_text()
            .contains("nothing is returned rather than")
    );
}

#[tokio::test]
async fn code_refs_names_its_unit_per_direction() {
    let (_directory, group) = group().await;
    let callers = group
        .code_refs(
            CodeRefsInput {
                symbol: "normalize_sku".to_owned(),
                direction: Direction::Callers,
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("refs")
        .expect("resolved");
    assert_eq!(callers.total, 3);
    assert_eq!(callers.unit, "referencing symbol");

    let callees = group
        .code_refs(
            CodeRefsInput {
                symbol: "add_item".to_owned(),
                direction: Direction::Callees,
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("refs")
        .expect("resolved");
    assert_eq!(callees.unit, "referenced symbol");
    assert_eq!(
        callees
            .references
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["normalize_sku"]
    );
}

#[tokio::test]
async fn an_unknown_symbol_is_refused_with_candidates_not_answered_with_zero() {
    let (_directory, group) = group().await;
    let refusal = group
        .code_refs(
            CodeRefsInput {
                symbol: "normalize_sku_typo".to_owned(),
                direction: Direction::Callers,
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("call succeeds")
        .expect_err("selector does not resolve");

    assert!(refusal.refused);
    assert!(
        refusal.did_you_mean.contains(&"normalize_sku".to_owned()),
        "{:?}",
        refusal.did_you_mean
    );
    assert!(refusal.symbols_known > 0);
}

#[tokio::test]
async fn a_symbol_that_exists_with_no_callers_is_a_zero_not_a_refusal() {
    // The distinction the refusal type exists to preserve.
    let (_directory, group) = group().await;
    let output = group
        .code_refs(
            CodeRefsInput {
                symbol: "orphan_helper".to_owned(),
                direction: Direction::Callers,
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("refs")
        .expect("orphan_helper is defined, so this must resolve");
    assert_eq!(output.total, 0);
    assert!(output.model_text().contains("not a proof"));
}

#[tokio::test]
async fn code_impact_reports_hop_distance_and_the_tests_that_reach_it() {
    let (_directory, group) = group().await;
    let output = group
        .code_impact(
            CodeImpactInput {
                symbol: "normalize_sku".to_owned(),
                depth: Some(3),
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("impact")
        .expect("resolved");

    assert!(output.total >= 3, "three direct callers at least");
    let direct: Vec<_> = output
        .reached
        .iter()
        .filter(|row| row.hops == 1)
        .map(|row| row.symbol.name.as_str())
        .collect();
    assert!(direct.contains(&"add_item"), "{direct:?}");
    assert!(
        output.reached.iter().any(|row| row.hops == 2),
        "the test reaching add_item sits two hops from normalize_sku"
    );
    assert!(
        output
            .tests_reaching
            .iter()
            .any(|row| row.symbol.name == "covers_add_item"),
        "existing coverage must be named: {:?}",
        output.tests_reaching
    );
}

#[tokio::test]
async fn code_expand_returns_the_body_and_its_neighbours() {
    let (_directory, group) = group().await;
    let output = group
        .code_expand(
            CodeExpandInput {
                symbol: "add_item".to_owned(),
                path: None,
            },
            None,
            &token(),
        )
        .await
        .expect("expand")
        .expect("resolved");

    assert_eq!(output.path, "src/catalog.rs");
    assert!(output.source.contains("normalize_sku(sku)"));
    assert!(
        output
            .callees
            .iter()
            .any(|symbol| symbol.name == "normalize_sku")
    );
}

#[tokio::test]
async fn code_expand_serves_the_whole_file_when_the_bundle_would_cost_more() {
    // Ripwire's honest counterexample: a summary that costs more than the thing it summarizes is
    // strictly worse, so the file is served and the result says why.
    let directory = TempDir::new().expect("temp dir");
    std::fs::write(
        directory.path().join("tiny.rs"),
        "pub fn only_function() -> u32 { 42 }\n",
    )
    .expect("write");
    let group = CodeGraphToolGroup::new(directory.path(), None)
        .await
        .expect("group");

    let output = group
        .code_expand(
            CodeExpandInput {
                symbol: "only_function".to_owned(),
                path: None,
            },
            None,
            &token(),
        )
        .await
        .expect("expand")
        .expect("resolved");

    assert!(
        output.served_whole_file.is_some(),
        "a symbol occupying most of a small file should come with the file"
    );
    assert!(output.model_text().contains("[whole file:"));
}

#[tokio::test]
async fn a_qualified_selector_picks_between_same_named_definitions() {
    let directory = TempDir::new().expect("temp dir");
    std::fs::create_dir_all(directory.path().join("src")).expect("mkdir");
    std::fs::write(directory.path().join("src/alpha.rs"), "pub fn handler() {}").expect("write");
    std::fs::write(directory.path().join("src/beta.rs"), "pub fn handler() {}").expect("write");
    let group = CodeGraphToolGroup::new(directory.path(), None)
        .await
        .expect("group");

    let both = group
        .code_refs(
            CodeRefsInput {
                symbol: "handler".to_owned(),
                direction: Direction::Callers,
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("refs")
        .expect("resolved");
    assert_eq!(
        both.matched.len(),
        2,
        "an unqualified name matching two definitions returns their union"
    );
    assert!(both.model_text().contains("names 2 definitions"));

    let one = group
        .code_refs(
            CodeRefsInput {
                symbol: "src/beta.rs::handler".to_owned(),
                direction: Direction::Callers,
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect("refs")
        .expect("resolved");
    assert_eq!(one.matched.len(), 1);
    assert_eq!(one.matched[0].path, "src/beta.rs");
}

#[tokio::test]
async fn repeated_calls_return_byte_identical_results() {
    // The cache is shared across calls. If it could change an answer, this is where it would show.
    let (_directory, group) = group().await;
    let first = group
        .code_map(CodeMapInput::default(), None, &token())
        .await
        .expect("map");
    let second = group
        .code_map(CodeMapInput::default(), None, &token())
        .await
        .expect("map");
    assert_eq!(
        serde_json::to_string(&first).expect("json"),
        serde_json::to_string(&second).expect("json")
    );
    assert_eq!(first.model_text(), second.model_text());
}

#[tokio::test]
async fn a_limit_narrows_the_result_and_the_truncation_is_disclosed() {
    let (_directory, group) = group().await;
    let output = group
        .code_map(
            CodeMapInput {
                path: None,
                limit: Some(2),
            },
            None,
            &token(),
        )
        .await
        .expect("map");
    assert_eq!(output.symbols.len(), 2);
    assert!(output.truncated);
    assert!(output.total > 2);
    assert!(output.model_text().contains("[truncated: showing 2 of"));
}

#[tokio::test]
async fn an_empty_task_is_rejected_rather_than_matching_everything() {
    let (_directory, group) = group().await;
    let error = group
        .code_context(
            CodeContextInput {
                task: "   ".to_owned(),
                path: None,
                limit: None,
            },
            None,
            &token(),
        )
        .await
        .expect_err("an empty task is not a query");
    assert_eq!(error.kind(), "invalid");
}

/// Records every phase a call reports, in arrival order.
#[derive(Default)]
struct RecordingSink {
    phases: std::sync::Mutex<Vec<GraphPhase>>,
}

#[async_trait::async_trait]
impl GraphProgressSink for RecordingSink {
    async fn publish(&self, progress: GraphProgress) {
        self.phases.lock().expect("sink").push(progress.phase);
    }
}

#[tokio::test]
async fn a_call_reports_its_phases_in_order() {
    const EXPECTED: &[GraphPhase] = &[GraphPhase::Crawl, GraphPhase::Parse, GraphPhase::Rank];

    let (_directory, group) = group().await;
    let sink = RecordingSink::default();
    group
        .code_map(CodeMapInput::default(), Some(&sink), &token())
        .await
        .expect("map");

    let phases = sink.phases.lock().expect("sink").clone();
    assert_eq!(
        phases, EXPECTED,
        "a host renders these in order; out-of-order or missing phases would animate backwards"
    );
}

#[tokio::test]
async fn a_call_without_a_sink_still_answers() {
    let (_directory, group) = group().await;
    let output = group
        .code_map(CodeMapInput::default(), None, &token())
        .await
        .expect("map");
    assert!(
        !output.symbols.is_empty(),
        "progress is advisory and must never gate the result"
    );
}
