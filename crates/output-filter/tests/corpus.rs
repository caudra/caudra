//! Executes every inline expectation shipped with the vendored rule corpus.
//!
//! The expectations are authored upstream beside the rules they describe. Running
//! them here makes a corpus refresh self-verifying: copying new rule files in and
//! running the suite proves the engine still reproduces the authored behavior,
//! without anyone re-deriving what each rule is supposed to do.

use workcell_output_filter::builtin;

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
