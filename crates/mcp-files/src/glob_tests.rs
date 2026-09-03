use proptest::{
    prelude::*,
    test_runner::{Config as ProptestConfig, RngSeed},
};

use crate::{
    FilesystemError, FilesystemLimits,
    glob::{GlobMatcher, MatchOutcome, MatchScratch},
};

fn matcher(pattern: &str) -> GlobMatcher {
    GlobMatcher::new(pattern, &FilesystemLimits::default()).expect("valid glob")
}

fn is_match(matcher: &GlobMatcher, value: &str) -> bool {
    let mut budget = usize::MAX;
    matcher
        .try_match(value, &mut budget, &mut MatchScratch::default())
        .expect("matching budget")
        == MatchOutcome::Matched
}

fn ascii_path() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[-a-c0-2._/]{0,24}").expect("ASCII path regex is valid")
}

fn ascii_glob() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            6 => proptest::string::string_regex("[-a-c0-2._/]{1}")
                .expect("ASCII glob literal regex is valid"),
            2 => Just("*".to_owned()),
            1 => Just("**".to_owned()),
            1 => Just("**/".to_owned()),
            1 => Just("?".to_owned()),
        ],
        0..16,
    )
    .prop_map(|parts| parts.concat())
}

fn reference_glob_match(pattern: &str, value: &str) -> bool {
    fn matches_from(
        pattern: &[u8],
        value: &[u8],
        pattern_index: usize,
        value_index: usize,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[pattern_index][value_index] {
            return result;
        }
        let result = if pattern_index == pattern.len() {
            value_index == value.len()
        } else if pattern[pattern_index..].starts_with(b"**/") {
            matches_from(pattern, value, pattern_index + 3, value_index, memo)
                || ((value_index + 1)..=value.len()).any(|end| {
                    value[end - 1] == b'/'
                        && matches_from(pattern, value, pattern_index + 3, end, memo)
                })
        } else if pattern[pattern_index..].starts_with(b"**") {
            matches_from(pattern, value, pattern_index + 2, value_index, memo)
                || (value_index < value.len()
                    && matches_from(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index] == b'*' {
            matches_from(pattern, value, pattern_index + 1, value_index, memo)
                || (value_index < value.len()
                    && value[value_index] != b'/'
                    && matches_from(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index] == b'?' {
            value_index < value.len()
                && value[value_index] != b'/'
                && matches_from(pattern, value, pattern_index + 1, value_index + 1, memo)
        } else {
            value_index < value.len()
                && pattern[pattern_index] == value[value_index]
                && matches_from(pattern, value, pattern_index + 1, value_index + 1, memo)
        };
        memo[pattern_index][value_index] = Some(result);
        result
    }

    let mut memo = vec![vec![None; value.len() + 1]; pattern.len() + 1];
    matches_from(pattern.as_bytes(), value.as_bytes(), 0, 0, &mut memo)
}

#[test]
fn supports_recursive_globs_braces_and_utf16_wildcards() {
    let recursive = matcher("**/*.{txt,md}");
    assert!(is_match(&recursive, "notes.txt"));
    assert!(is_match(&recursive, "nested/readme.md"));
    assert!(!is_match(&recursive, "image.png"));
    assert!(!is_match(&matcher("?"), "😀"));
    assert!(is_match(&matcher("??"), "😀"));
}

#[test]
fn rejects_expansion_before_exponential_allocation() {
    let limits = FilesystemLimits {
        max_glob_alternatives: 8,
        ..FilesystemLimits::default()
    };
    let error = GlobMatcher::new("{a,b}{a,b}{a,b}{a,b}", &limits)
        .expect_err("sixteen alternatives must be rejected");
    assert!(matches!(error, FilesystemError::Operation(_)));
    assert!(error.to_string().contains("maximum of 8 alternatives"));

    for (limits, pattern, expected) in [
        (
            FilesystemLimits {
                max_glob_bytes: 3,
                ..FilesystemLimits::default()
            },
            "four",
            "maximum size",
        ),
        (
            FilesystemLimits {
                max_glob_brace_depth: 1,
                ..FilesystemLimits::default()
            },
            "{a,b}{c,d}",
            "brace depth",
        ),
        (
            FilesystemLimits {
                max_glob_generated_bytes: 3,
                ..FilesystemLimits::default()
            },
            "{ab,cd}",
            "expansion exceeds",
        ),
    ] {
        assert!(
            GlobMatcher::new(pattern, &limits)
                .expect_err("bounded glob")
                .to_string()
                .contains(expected)
        );
    }

    // Budget exhaustion is a truncation signal, not an error: callers report the
    // results they already collected instead of discarding all of them.
    let bounded = matcher("**/*");
    let mut one_step = 1;
    assert_eq!(
        bounded
            .try_match(
                "nested/file.txt",
                &mut one_step,
                &mut MatchScratch::default()
            )
            .expect("budget exhaustion is not an error"),
        MatchOutcome::BudgetExhausted
    );
    assert_eq!(one_step, 1, "an exhausted attempt must not consume budget");
}

#[test]
fn literal_paths_stay_within_the_default_budget_in_large_trees() {
    const CANDIDATE_COUNT: usize = 10_000;
    const MATCHING_PATH: &str = "src/cmd/tui.rs";
    const CANDIDATE_BASENAME: &str = "dependency.rlib";

    for pattern in [MATCHING_PATH, "**/cmd/tui.rs"] {
        let matcher = matcher(pattern);
        let mut scratch = MatchScratch::default();
        let mut budget = FilesystemLimits::default().max_glob_match_steps;
        assert_eq!(
            matcher
                .try_match(MATCHING_PATH, &mut budget, &mut scratch)
                .expect("matching path"),
            MatchOutcome::Matched
        );

        for index in 0..CANDIDATE_COUNT {
            let candidate = format!("target/debug/deps/dependency-{index}.rlib");
            assert_eq!(
                matcher
                    .matches_candidate(&candidate, CANDIDATE_BASENAME, &mut budget, &mut scratch)
                    .expect("non-matching candidate"),
                MatchOutcome::Missed
            );
        }
    }
}

/// The default work budget must cover the ordinary wildcard patterns the tool
/// descriptions advertise, evaluated against a full traversal budget worth of
/// candidates.
///
/// The pre-existing budget regression covered only literal and `**/<literal>`
/// patterns, which take the cheap fast path. Every advertised pattern with a
/// wildcard in its suffix uses the quadratic matcher instead, and that is the
/// shape that used to exhaust the budget on an ordinary repository.
#[test]
fn wildcard_patterns_stay_within_the_default_budget_at_traversal_scale() {
    let limits = FilesystemLimits::default();
    let corpus = synthetic_corpus(limits.max_traversal_entries);
    for pattern in [
        "**/*.rs",
        "src/**/*.ts",
        "**/*.{ts,tsx}",
        "**/*.{ts,tsx,js,jsx,mjs,cjs}",
        "**/test_*.py",
    ] {
        let matcher = matcher(pattern);
        assert!(
            !matcher.debug_uses_fast_path(),
            "{pattern} must exercise the quadratic matcher, not the cheap fast path"
        );
        let mut scratch = MatchScratch::default();
        let mut budget = limits.max_glob_match_steps;
        for candidate in &corpus {
            let basename = candidate.rsplit('/').next().unwrap_or(candidate);
            assert_ne!(
                matcher
                    .matches_candidate(candidate, basename, &mut budget, &mut scratch)
                    .expect("bounded matching"),
                MatchOutcome::BudgetExhausted,
                "{pattern} exhausted the default budget before the traversal budget"
            );
        }
    }
}

/// Skipping the basename retry must never change which candidates match.
///
/// A `**/`-anchored pattern accepts an empty prefix or any prefix ending in
/// `/`, so a basename hit implies a relative-path hit and the retry is pure
/// waste. This pins that reasoning against the unoptimized behaviour.
#[test]
fn skipping_the_basename_retry_preserves_match_semantics() {
    for pattern in [
        "**/*.rs",
        "**/*.{ts,tsx}",
        "**/cmd/tui.rs",
        "*.rs",
        "src/*.rs",
        "{**/*.ts,src/*.js}",
    ] {
        let matcher = matcher(pattern);
        for candidate in [
            "main.rs",
            "src/main.rs",
            "a/b/c/deep.rs",
            "src/app.ts",
            "src/app.js",
            "cmd/tui.rs",
            "src/cmd/tui.rs",
            "notes.txt",
        ] {
            let basename = candidate.rsplit('/').next().unwrap_or(candidate);
            let mut optimized_budget = usize::MAX;
            let optimized = matcher
                .matches_candidate(
                    candidate,
                    basename,
                    &mut optimized_budget,
                    &mut MatchScratch::default(),
                )
                .expect("bounded matching");

            let mut reference_budget = usize::MAX;
            let mut scratch = MatchScratch::default();
            let reference = if is_match(&matcher, candidate) {
                MatchOutcome::Matched
            } else {
                matcher
                    .try_match(basename, &mut reference_budget, &mut scratch)
                    .expect("bounded matching")
            };
            assert_eq!(
                optimized, reference,
                "pattern={pattern:?} candidate={candidate:?}"
            );

            if matcher.debug_subsumes_basename() {
                assert!(
                    !(is_match(&matcher, basename) && !is_match(&matcher, candidate)),
                    "pattern={pattern:?} claims subsumption but basename {basename:?} \
                     matches while path {candidate:?} does not"
                );
            }
        }
    }
}

fn synthetic_corpus(count: usize) -> Vec<String> {
    // Shapes sampled to reproduce the measured distribution across 34 real
    // repositories: mean 47.7, p50 43, p90 72, p99 122, max 167.
    const DIRS: &[&str] = &[
        "src",
        "crates",
        "lib",
        "tests",
        "internal",
        "packages",
        "components",
        "read_operations",
        "mutation_operations",
        "very_long_generated_module_directory_name",
    ];
    const STEMS: &[&str] = &[
        "mod",
        "index",
        "traversal",
        "glob",
        "handler",
        "test_client",
        "a_rather_long_generated_source_file_name_for_tail_coverage",
    ];
    const EXTS: &[&str] = &["rs", "ts", "tsx", "js", "py", "json", "rlib", "md"];
    let mut out = Vec::with_capacity(count);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as usize
    };
    for _ in 0..count {
        let depth = 1 + next() % 5;
        let mut path = String::new();
        for _ in 0..depth {
            path.push_str(DIRS[next() % DIRS.len()]);
            path.push('/');
        }
        path.push_str(STEMS[next() % STEMS.len()]);
        path.push('.');
        path.push_str(EXTS[next() % EXTS.len()]);
        out.push(path);
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x5eed_610b),
        ..ProptestConfig::default()
    })]

    #[test]
    fn translated_globs_match_reference_semantics(pattern in ascii_glob(), path in ascii_path()) {
        let matcher = matcher(&pattern);
        prop_assert_eq!(
            is_match(&matcher, &path),
            reference_glob_match(&pattern, &path),
            "pattern={:?}, path={:?}",
            pattern,
            path,
        );
    }

    #[test]
    fn brace_expansion_matches_the_union_of_alternatives(
        prefix in "[-a-c0-2._/]{0,8}",
        left in "[-a-c0-2._/]{1,8}",
        right in "[-a-c0-2._/]{1,8}",
        suffix in "[-a-c0-2._/]{0,8}",
        path in ascii_path(),
    ) {
        let brace = matcher(&format!("{prefix}{{{left},{right}}}{suffix}"));
        let left = matcher(&format!("{prefix}{left}{suffix}"));
        let right = matcher(&format!("{prefix}{right}{suffix}"));

        prop_assert_eq!(
            is_match(&brace, &path),
            is_match(&left, &path) || is_match(&right, &path),
        );
    }
}
