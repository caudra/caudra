//! Executes every inline expectation shipped with the vendored rule corpus.
//!
//! The expectations are authored upstream beside the rules they describe. Running
//! them here makes a corpus refresh self-verifying: copying new rule files in and
//! running the suite proves the engine still reproduces the authored behavior,
//! without anyone re-deriving what each rule is supposed to do.

use workcell_output_filter::{Rule, builtin};

#[test]
fn every_inline_expectation_holds() {
    let corpus = builtin();
    let mut executed = 0usize;
    let mut failures = Vec::new();

    for (name, cases) in corpus.tests() {
        let Some(rule) = corpus.rule(name) else {
            failures.push(format!("{name}: expectations exist but no rule is defined"));
            continue;
        };
        for case in cases {
            executed += 1;
            // Upstream authors expectations against successful runs; the failure
            // path is covered separately because it is a local divergence.
            let actual = rule.apply(&case.input, "", Some(0)).text;
            // TOML multi-line strings carry a trailing newline that the authored
            // expectation does not intend as output. Upstream normalizes both
            // sides the same way before comparing, so this is a property of the
            // fixture format rather than a tolerated behavioral difference.
            let actual = actual.trim_end_matches('\n');
            let expected = case.expected.trim_end_matches('\n');
            if actual != expected {
                failures.push(format!(
                    "{name} / {}:\n  expected: {expected:?}\n  actual:   {actual:?}",
                    case.name
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {executed} inline expectations failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(executed > 0, "corpus shipped no inline expectations");
}

#[test]
fn rules_match_normalized_scopes_rather_than_raw_command_strings() {
    let corpus = builtin();
    // An absolute path is normalized to its basename before matching, so a rule
    // applies to the program it names however that program was invoked.
    assert!(corpus.find("liquibase update").is_some());
    assert!(corpus.find("df -h").is_some());
    // A different program with a matching prefix must not be captured.
    assert!(corpus.find("dfu-util --list").is_none());
}

#[test]
fn nextest_run_is_filtered_and_its_listing_subcommands_are_not() {
    let corpus = builtin();
    // `cargo test` and `cargo nextest run` produce unrelated formats, so each
    // must reach its own rule rather than whichever sorts first.
    assert_eq!(
        corpus.find("cargo test --workspace").map(Rule::name),
        Some("cargo-test")
    );
    assert_eq!(
        corpus.find("cargo nextest run --workspace").map(Rule::name),
        Some("cargo-nextest")
    );
    // Listing test names is the result the caller asked for, not progress, so
    // no rule may claim it. The same holds for the other nextest subcommands,
    // none of which report a test run.
    assert!(corpus.find("cargo nextest list").is_none());
    assert!(
        corpus
            .find("cargo nextest list --list-type binaries-only")
            .is_none()
    );
    assert!(
        corpus
            .find("cargo nextest archive --archive-file a.tar.zst")
            .is_none()
    );
    assert!(
        corpus
            .find("cargo nextest show-config test-groups")
            .is_none()
    );
}

#[test]
fn nextest_is_reduced_on_the_stream_it_actually_writes_to() {
    // nextest writes its whole report to stderr, but inline expectations are fed
    // through stdout. Without this, dropping `filter_stderr` would keep every
    // inline case green while leaving real runs entirely unfiltered.
    let rule = builtin().rule("cargo-nextest").expect("cargo-nextest rule");
    let stderr = concat!(
        "────────────\n",
        " Nextest run ID 965dcdd4-7746-4611-8ab8-f72410730bd6 with nextest profile: default\n",
        "    Starting 4 tests across 1 binary (1 test skipped)\n",
        "        PASS [   0.028s] (1/4) rustdemo tests::adds\n",
        "        PASS [   1.537s] (4/4) rustdemo tests::slow_one\n",
        "────────────\n",
        "     Summary [   1.541s] 4 tests run: 4 passed, 1 skipped\n",
    );

    let filtered = rule.apply("", stderr, Some(0));

    assert!(filtered.consumed_stderr);
    assert_eq!(
        filtered.text,
        "     Summary [   1.541s] 4 tests run: 4 passed, 1 skipped"
    );
}

#[test]
fn cargo_test_reduces_large_passing_suites_to_their_summary() {
    const TEST_COUNT: usize = 86;
    let mut stdout = format!("running {TEST_COUNT} tests\n");
    for test in 0..TEST_COUNT {
        stdout.push_str(&format!("test sdk_mode::tests::case_{test} ... ok\n"));
    }
    stdout.push_str(
        "test result: ok. 86 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.99s\n",
    );
    let rule = builtin().rule("cargo-test").expect("cargo-test rule");

    let filtered = rule.apply(&stdout, "warning: future incompatibility\n", Some(0));

    assert!(!filtered.text.contains("case_42"));
    assert!(filtered.text.contains("test result: ok. 86 passed"));
    assert!(filtered.text.contains("warning: future incompatibility"));
    assert!(filtered.text.len() < stdout.len() / 4);
}
