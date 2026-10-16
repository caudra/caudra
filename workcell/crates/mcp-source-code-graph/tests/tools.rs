//! End-to-end behaviour of the two tools against a real tree on disk.

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_files::{FileResource, FileResourceAccess};
use workcell_mcp_source_code_graph::{
    Direction, GraphPhase, GraphProgress, GraphProgressSink, ModelText, RankedSymbol,
    SelectorRefusal, SourceCodeContextInput, SourceCodeContextOutput, SourceCodeGraphToolGroup,
    SourceCodeRefsInput, SourceCodeRefsOutput,
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

async fn group() -> (TempDir, SourceCodeGraphToolGroup) {
    let directory = TempDir::new().expect("temp dir");
    for (path, contents) in TREE {
        let full = directory.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(full, contents).expect("write");
    }
    let group = SourceCodeGraphToolGroup::new(directory.path(), None)
        .await
        .expect("group");
    (directory, group)
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

const NORMALIZE: &str = "normalize_sku";
const NORMALIZE_TASK: &str = "normalize a sku";

fn refs_input(symbol: &str, direction: Direction, depth: Option<usize>) -> SourceCodeRefsInput {
    SourceCodeRefsInput {
        symbol: symbol.to_owned(),
        direction,
        depth,
        path: None,
        limit: None,
    }
}

fn context_input(task: &str, path: Option<&str>, limit: Option<usize>) -> SourceCodeContextInput {
    SourceCodeContextInput {
        task: task.to_owned(),
        path: path.map(str::to_owned),
        limit,
    }
}

async fn refs(
    group: &SourceCodeGraphToolGroup,
    input: SourceCodeRefsInput,
) -> Result<SourceCodeRefsOutput, SelectorRefusal> {
    group
        .source_code_refs(input, None, &token())
        .await
        .expect("refs runs")
}

async fn context(
    group: &SourceCodeGraphToolGroup,
    input: SourceCodeContextInput,
) -> SourceCodeContextOutput {
    group
        .source_code_context(input, None, &token())
        .await
        .expect("context runs")
}

fn names(rows: &[workcell_mcp_source_code_graph::ReferenceRow]) -> Vec<&str> {
    rows.iter().map(|row| row.symbol.name.as_str()).collect()
}

#[tokio::test]
async fn source_code_refs_lists_every_caller_with_its_call_site() {
    let (_directory, group) = group().await;
    let output = refs(&group, refs_input(NORMALIZE, Direction::Callers, None))
        .await
        .expect("resolved");

    assert!(output.graph.files_indexed >= 5);
    assert!(output.counts_floor, "every count here is a floor");
    assert_eq!(output.total, 3, "three distinct callers");
    assert_eq!(output.depth, 1);
    assert!(
        output.by_depth.is_empty(),
        "a one-hop answer carries no per-hop summary"
    );
    let add_item = output
        .references
        .iter()
        .find(|row| row.symbol.name == "add_item")
        .expect("add_item calls normalize_sku");
    assert_eq!(add_item.hops, 1);
    assert_eq!(
        add_item
            .sites
            .iter()
            .map(|site| (site.path.as_str(), site.line))
            .collect::<Vec<_>>(),
        [("src/catalog.rs", 2)]
    );
    assert!(
        output.model_text().contains("at=2"),
        "{}",
        output.model_text()
    );
}

#[tokio::test]
async fn preparation_binds_validated_options_without_crawling() {
    let (_directory, group) = group().await;
    let refs = group
        .prepare_source_code_refs(
            SourceCodeRefsInput {
                limit: Some(usize::MAX),
                ..refs_input(NORMALIZE, Direction::Callers, Some(usize::MAX))
            },
            group.inspect_scope(None).await.expect("scope"),
        )
        .expect("prepare refs");
    assert_eq!(refs.symbol(), NORMALIZE);
    assert_eq!(refs.direction(), Direction::Callers);
    assert_eq!(refs.depth(), 8);
    assert_eq!(refs.limit(), group.limits().max_result_limit);
    assert_eq!(refs.path(), ".");
    assert!(refs.retained_bytes() >= refs.symbol().len() + refs.path().len());

    let context = group
        .prepare_source_code_context(
            context_input("find normalization", None, Some(7)),
            group.inspect_scope(None).await.expect("scope"),
        )
        .expect("prepare context");
    assert_eq!(context.task(), "find normalization");
    assert_eq!(context.limit(), 7);
    assert!(context.retained_bytes() >= context.task().len() + context.path().len());

    assert!(
        group
            .prepare_source_code_context(
                context_input("   ", None, None),
                group.inspect_scope(None).await.expect("scope"),
            )
            .is_err()
    );
}

#[tokio::test]
async fn a_walk_over_callees_is_refused_as_invalid() {
    let (_directory, group) = group().await;
    let error = group
        .source_code_refs(
            refs_input("add_item", Direction::Callees, Some(2)),
            None,
            &token(),
        )
        .await
        .expect_err("callees have no depth");
    assert_eq!(error.kind(), "invalid");
}

#[tokio::test]
async fn prepared_execution_uses_the_host_supplied_scope_not_the_raw_selector() {
    let (directory, group) = group().await;
    let prepared = group
        .prepare_source_code_context(
            context_input(NORMALIZE_TASK, Some("tests"), None),
            FileResource {
                requested_path: "host-authorized-src".to_owned(),
                path: directory.path().join("src"),
                root_relative_path: "src".to_owned(),
                access: FileResourceAccess::Traverse,
            },
        )
        .expect("prepare context");
    assert_eq!(prepared.path(), "tests");
    assert!(prepared.retained_bytes() >= prepared.path().len());

    let output = group
        .execute_prepared_source_code_context(prepared, None, &token())
        .await
        .expect("execute prepared context");
    assert!(output.results.iter().any(|symbol| symbol.name == NORMALIZE));
    assert!(
        output
            .results
            .iter()
            .all(|symbol| !symbol.path.starts_with("tests/"))
    );
}

#[tokio::test]
async fn prepared_execution_fails_when_the_authorized_scope_is_stale() {
    let (directory, group) = group().await;
    let scope = group.inspect_scope(Some("src")).await.expect("scope");
    let prepared = group
        .prepare_source_code_context(context_input(NORMALIZE_TASK, Some("src"), None), scope)
        .expect("prepare context");
    std::fs::rename(directory.path().join("src"), directory.path().join("moved"))
        .expect("move scope");

    assert!(
        group
            .execute_prepared_source_code_context(prepared, None, &token())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn paths_are_root_relative_whether_or_not_the_graph_is_scoped() {
    let (_directory, group) = group().await;
    let whole = context(&group, context_input(NORMALIZE_TASK, None, None)).await;
    let scoped = context(&group, context_input(NORMALIZE_TASK, Some("src"), None)).await;

    let find = |symbols: &[RankedSymbol]| {
        symbols
            .iter()
            .find(|symbol| symbol.name == NORMALIZE)
            .map(|symbol| symbol.path.clone())
    };
    assert_eq!(
        find(&whole.results),
        find(&scoped.results),
        "a path from a scoped search must be usable against the whole tree"
    );
    assert_eq!(find(&scoped.results).as_deref(), Some("src/normalize.rs"));
    assert!(
        scoped.graph.files_indexed < whole.graph.files_indexed,
        "scoping must actually narrow the crawl"
    );
}

/// Models reach for `""` when they mean "no scope", and this group used to answer that with an error
/// borrowed from `file_glob` — naming a parameter of a tool the caller never invoked. Absent already
/// means the whole root, so the empty string has one reading and it must be indistinguishable from
/// it, in the echoed `path` as much as in the ranking.
#[tokio::test]
async fn an_empty_path_is_indistinguishable_from_an_absent_one() {
    let (_directory, group) = group().await;

    let absent = context(&group, context_input(NORMALIZE_TASK, None, None)).await;
    let empty = context(&group, context_input(NORMALIZE_TASK, Some(""), None)).await;
    assert_eq!(format!("{empty:?}"), format!("{absent:?}"));
    assert_eq!(
        empty.path, ".",
        "an empty scope echoes the root, not itself"
    );

    // A second tool, because the normalization is applied per call site and one passing site proves
    // nothing about the other.
    let absent = refs(&group, refs_input(NORMALIZE, Direction::Callers, None))
        .await
        .expect("selector resolves");
    let empty = refs(
        &group,
        SourceCodeRefsInput {
            path: Some(String::new()),
            ..refs_input(NORMALIZE, Direction::Callers, None)
        },
    )
    .await
    .expect("selector resolves");
    assert_eq!(format!("{empty:?}"), format!("{absent:?}"));
}

#[tokio::test]
async fn source_code_context_finds_a_symbol_the_task_did_not_spell() {
    let (_directory, group) = group().await;
    let output = context(
        &group,
        context_input("where do we invalidate a cache entry", None, Some(5)),
    )
    .await;

    assert_eq!(output.shape, "conceptual");
    assert_eq!(
        output.results.first().map(|symbol| symbol.name.as_str()),
        Some("invalidate_cache_entry")
    );
    assert!(output.estimated_tokens > 0);
}

#[tokio::test]
async fn source_code_context_returns_nothing_for_an_unrelated_task() {
    // The alternative is returning the repository's busiest symbol, which answers a question that
    // was not asked and reads exactly like a real answer.
    let (_directory, group) = group().await;
    let output = context(
        &group,
        context_input("quantum chromodynamics lattice solver", None, Some(5)),
    )
    .await;
    assert!(output.results.is_empty(), "{:?}", output.results);
    assert_eq!(output.total_matched, 0);
    assert!(
        output
            .model_text()
            .contains("nothing is returned rather than")
    );
}

#[tokio::test]
async fn source_code_refs_names_its_unit_per_direction() {
    let (_directory, group) = group().await;
    let callers = refs(&group, refs_input(NORMALIZE, Direction::Callers, None))
        .await
        .expect("resolved");
    assert_eq!(callers.total, 3);
    assert_eq!(callers.unit, "referencing symbol");

    let callees = refs(&group, refs_input("add_item", Direction::Callees, None))
        .await
        .expect("resolved");
    assert_eq!(callees.unit, "referenced symbol");
    assert_eq!(names(&callees.references), [NORMALIZE]);
}

#[tokio::test]
async fn callees_come_in_the_order_the_body_calls_them() {
    // Rank order would lead with `first`, which two symbols call; the body calls `second` first.
    let directory = TempDir::new().expect("temp dir");
    std::fs::write(
        directory.path().join("lib.rs"),
        "pub fn first() {}\npub fn second() {}\npub fn driver() {\n    second();\n    first();\n}\npub fn other() { first(); }\n",
    )
    .expect("write");
    let group = SourceCodeGraphToolGroup::new(directory.path(), None)
        .await
        .expect("group");
    let callees = refs(&group, refs_input("driver", Direction::Callees, None))
        .await
        .expect("resolved");
    assert_eq!(names(&callees.references), ["second", "first"]);
    assert!(
        callees
            .model_text()
            .contains("second function lib.rs:2-2 in=1 out=0 at=4")
    );
}

#[tokio::test]
async fn an_unknown_symbol_is_refused_with_candidates_not_answered_with_zero() {
    let (_directory, group) = group().await;
    let refusal = refs(
        &group,
        refs_input("normalize_sku_typo", Direction::Callers, None),
    )
    .await
    .expect_err("selector does not resolve");

    assert!(refusal.refused);
    assert!(
        refusal.did_you_mean.contains(&NORMALIZE.to_owned()),
        "{:?}",
        refusal.did_you_mean
    );
    assert!(refusal.symbols_known > 0);
}

#[tokio::test]
async fn a_symbol_that_exists_with_no_callers_is_a_zero_not_a_refusal() {
    // The distinction the refusal type exists to preserve.
    let (_directory, group) = group().await;
    let output = refs(
        &group,
        refs_input("orphan_helper", Direction::Callers, None),
    )
    .await
    .expect("orphan_helper is defined, so this must resolve");
    assert_eq!(output.total, 0);
    assert!(output.model_text().contains("not a proof"));
}

#[tokio::test]
async fn a_walk_reports_hop_distance_and_the_tests_that_reach_it() {
    let (_directory, group) = group().await;
    let output = refs(&group, refs_input(NORMALIZE, Direction::Callers, Some(3)))
        .await
        .expect("resolved");

    assert!(output.total >= 3, "three direct callers at least");
    let direct: Vec<_> = output
        .references
        .iter()
        .filter(|row| row.hops == 1)
        .map(|row| row.symbol.name.as_str())
        .collect();
    assert!(direct.contains(&"add_item"), "{direct:?}");
    let test = output
        .references
        .iter()
        .find(|row| row.symbol.name == "covers_add_item")
        .expect("the test reaching add_item sits two hops from normalize_sku");
    assert_eq!(test.hops, 2);
    assert!(test.symbol.test_scope);
    assert_eq!(output.tests_reaching, Some(1));
    assert_eq!(output.by_depth.iter().sum::<usize>(), output.total);
    assert_eq!(output.by_depth.len(), 3);
    assert!(
        output
            .references
            .windows(2)
            .all(|pair| pair[0].hops <= pair[1].hops),
        "nearest first"
    );
}

#[tokio::test]
async fn a_qualified_selector_picks_between_same_named_definitions() {
    let directory = TempDir::new().expect("temp dir");
    std::fs::create_dir_all(directory.path().join("src")).expect("mkdir");
    std::fs::write(directory.path().join("src/alpha.rs"), "pub fn handler() {}").expect("write");
    std::fs::write(directory.path().join("src/beta.rs"), "pub fn handler() {}").expect("write");
    let group = SourceCodeGraphToolGroup::new(directory.path(), None)
        .await
        .expect("group");

    let both = refs(&group, refs_input("handler", Direction::Callers, None))
        .await
        .expect("resolved");
    assert_eq!(
        both.matched.len(),
        2,
        "an unqualified name matching two definitions returns their union"
    );
    assert!(both.model_text().contains("names 2 definitions"));

    let one = refs(
        &group,
        refs_input("src/beta.rs::handler", Direction::Callers, None),
    )
    .await
    .expect("resolved");
    assert_eq!(one.matched.len(), 1);
    assert_eq!(one.matched[0].path, "src/beta.rs");
}

#[tokio::test]
async fn repeated_calls_return_byte_identical_results() {
    // The cache is shared across calls. If it could change an answer, this is where it would show.
    let (_directory, group) = group().await;
    let first = refs(&group, refs_input(NORMALIZE, Direction::Callers, Some(3)))
        .await
        .expect("resolved");
    let second = refs(&group, refs_input(NORMALIZE, Direction::Callers, Some(3)))
        .await
        .expect("resolved");
    assert_eq!(
        serde_json::to_string(&first).expect("json"),
        serde_json::to_string(&second).expect("json")
    );
    assert_eq!(first.model_text(), second.model_text());
}

#[tokio::test]
async fn a_limit_narrows_the_result_and_the_truncation_is_disclosed() {
    let (_directory, group) = group().await;
    let output = refs(
        &group,
        SourceCodeRefsInput {
            limit: Some(2),
            ..refs_input(NORMALIZE, Direction::Callers, None)
        },
    )
    .await
    .expect("resolved");
    assert_eq!(output.references.len(), 2);
    assert!(output.truncated);
    assert_eq!(output.total, 3);
    assert!(output.model_text().contains("[truncated: showing 2 of 3]"));
}

#[tokio::test]
async fn an_empty_task_is_rejected_rather_than_matching_everything() {
    let (_directory, group) = group().await;
    let error = group
        .source_code_context(context_input("   ", None, None), None, &token())
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
        .source_code_context(
            context_input(NORMALIZE_TASK, None, None),
            Some(&sink),
            &token(),
        )
        .await
        .expect("context");

    let phases = sink.phases.lock().expect("sink").clone();
    assert_eq!(
        phases, EXPECTED,
        "a host renders these in order; out-of-order or missing phases would animate backwards"
    );
}

#[tokio::test]
async fn a_call_without_a_sink_still_answers() {
    let (_directory, group) = group().await;
    let output = context(&group, context_input(NORMALIZE_TASK, None, None)).await;
    assert!(
        !output.results.is_empty(),
        "progress is advisory and must never gate the result"
    );
}
