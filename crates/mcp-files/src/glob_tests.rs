use proptest::{
    prelude::*,
    test_runner::{Config as ProptestConfig, RngSeed},
};

use crate::{FilesystemError, FilesystemLimits, glob::GlobMatcher};

fn matcher(pattern: &str) -> GlobMatcher {
    GlobMatcher::new(pattern, &FilesystemLimits::default()).expect("valid glob")
}

fn is_match(matcher: &GlobMatcher, value: &str) -> bool {
    let mut budget = usize::MAX;
    matcher
        .is_match(value, &mut budget)
        .expect("matching budget")
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

    let bounded = matcher("**/*");
    let mut one_step = 1;
    assert!(
        bounded
            .is_match("nested/file.txt", &mut one_step)
            .expect_err("matching work")
            .to_string()
            .contains("work budget")
    );
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
