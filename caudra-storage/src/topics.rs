//! Topic names and subscription patterns. A topic is a dot-separated path
//! such as `ci.failures`. A pattern may use `*` for exactly one segment and
//! a final `**` for one or more trailing segments. The message store matches
//! consumer group patterns with the same grammar sessions subscribe with.

pub const MAX_PATTERNS: usize = 16;
const MAX_TOPIC_BYTES: usize = 128;
const MAX_SEGMENTS: usize = 8;
const SEPARATOR: char = '.';
const ANY_SEGMENT: &str = "*";
const ANY_TAIL: &str = "**";
pub const INVALID_TOPIC: &str = "Topics have 1 to 8 dot-separated segments of lowercase letters, digits, hyphens, and underscores, each starting with a letter or digit, within 128 bytes";
pub const INVALID_PATTERN: &str = "Topic patterns use topic syntax, where * matches one segment and a final ** matches one or more";
pub const TOO_MANY_PATTERNS: &str = "A session subscribes to at most 16 topic patterns";
pub const MISSING_PATTERN: &str = "This session does not subscribe to the topic pattern";

fn valid_segment(segment: &str) -> bool {
    segment
        .bytes()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && segment.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn within_bounds(value: &str) -> bool {
    value.len() <= MAX_TOPIC_BYTES && value.split(SEPARATOR).count() <= MAX_SEGMENTS
}

/// Validates a concrete topic, which publishing requires.
pub fn parse_topic(value: &str) -> Result<String, String> {
    if within_bounds(value) && value.split(SEPARATOR).all(valid_segment) {
        Ok(value.to_owned())
    } else {
        Err(INVALID_TOPIC.into())
    }
}

pub fn parse_pattern(value: &str) -> Result<String, String> {
    let segments = value.split(SEPARATOR).count();
    let valid = within_bounds(value)
        && value.split(SEPARATOR).enumerate().all(|(index, segment)| {
            segment == ANY_SEGMENT
                || (segment == ANY_TAIL && index + 1 == segments)
                || valid_segment(segment)
        });
    if valid {
        Ok(value.to_owned())
    } else {
        Err(INVALID_PATTERN.into())
    }
}

/// Validates a whole subscription set, as a session stores or advertises it.
pub fn validate_patterns(patterns: &[String]) -> Result<(), String> {
    if patterns.len() > MAX_PATTERNS {
        return Err(TOO_MANY_PATTERNS.into());
    }
    patterns
        .iter()
        .try_for_each(|pattern| parse_pattern(pattern).map(drop))
}

/// `current` followed by each pattern of `added` it lacks.
pub fn add_patterns(current: &[String], added: &[String]) -> Result<Vec<String>, String> {
    let mut patterns = current.to_vec();
    for pattern in added {
        let pattern = parse_pattern(pattern)?;
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    validate_patterns(&patterns)?;
    Ok(patterns)
}

/// `current` without the patterns of `removed`, each of which it must hold.
pub fn remove_patterns(current: &[String], removed: &[String]) -> Result<Vec<String>, String> {
    if let Some(missing) = removed.iter().find(|pattern| !current.contains(pattern)) {
        return Err(format!("{MISSING_PATTERN}: {missing:?}"));
    }
    Ok(current
        .iter()
        .filter(|pattern| !removed.contains(pattern))
        .cloned()
        .collect())
}

/// Expects a pattern from [`parse_pattern`] and a topic from [`parse_topic`].
pub fn pattern_matches(pattern: &str, topic: &str) -> bool {
    let mut expected = pattern.split(SEPARATOR);
    let mut actual = topic.split(SEPARATOR);
    loop {
        match (expected.next(), actual.next()) {
            (Some(ANY_TAIL), Some(_)) | (None, None) => return true,
            (Some(ANY_SEGMENT), Some(_)) => {}
            (Some(expected), Some(actual)) if expected == actual => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        INVALID_PATTERN, INVALID_TOPIC, MAX_PATTERNS, MAX_TOPIC_BYTES, MISSING_PATTERN,
        TOO_MANY_PATTERNS, add_patterns, parse_pattern, parse_topic, pattern_matches,
        remove_patterns, validate_patterns,
    };

    const FAILURES: &str = "ci.failures";
    const EVERY_CI_TOPIC: &str = "ci.**";

    fn numbered(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("topic-{index}")).collect()
    }

    #[test_case("ci"; "single_segment")]
    #[test_case("ci.failures"; "two_segments")]
    #[test_case("build-1.unit_tests"; "digits_hyphens_and_underscores")]
    #[test_case("a.b.c.d.e.f.g.h"; "eight_segments")]
    fn valid_topics_parse(topic: &str) {
        assert_eq!(parse_topic(topic).unwrap(), topic);
        assert_eq!(parse_pattern(topic).unwrap(), topic);
    }

    #[test_case(""; "empty")]
    #[test_case(".ci"; "leading_separator")]
    #[test_case("ci."; "trailing_separator")]
    #[test_case("ci..failures"; "empty_segment")]
    #[test_case("CI.failures"; "uppercase")]
    #[test_case("ci.-failures"; "leading_hyphen")]
    #[test_case("ci._failures"; "leading_underscore")]
    #[test_case("ci failures"; "space")]
    #[test_case("ci.*"; "wildcard")]
    #[test_case("a.b.c.d.e.f.g.h.i"; "nine_segments")]
    fn invalid_topics_are_refused(topic: &str) {
        assert_eq!(parse_topic(topic).unwrap_err(), INVALID_TOPIC);
    }

    #[test_case("ci.*"; "one_segment_wildcard")]
    #[test_case("*.failures"; "leading_wildcard")]
    #[test_case("ci.**"; "trailing_wildcard")]
    #[test_case("**"; "every_topic")]
    fn valid_patterns_parse(pattern: &str) {
        assert_eq!(parse_pattern(pattern).unwrap(), pattern);
    }

    #[test_case(""; "empty")]
    #[test_case("ci."; "trailing_separator")]
    #[test_case("ci.**.failures"; "inner_trailing_wildcard")]
    #[test_case("ci.***"; "triple_star")]
    #[test_case("ci.fail*"; "partial_wildcard")]
    #[test_case("a.b.c.d.e.f.g.h.**"; "nine_segments")]
    fn invalid_patterns_are_refused(pattern: &str) {
        assert_eq!(parse_pattern(pattern).unwrap_err(), INVALID_PATTERN);
    }

    #[test_case(MAX_TOPIC_BYTES, true; "at_limit")]
    #[test_case(MAX_TOPIC_BYTES + 1, false; "over_limit")]
    fn topic_length_is_bounded(bytes: usize, valid: bool) {
        let topic = "a".repeat(bytes);
        assert_eq!(parse_topic(&topic).is_ok(), valid);
        assert_eq!(parse_pattern(&topic).is_ok(), valid);
    }

    #[test_case("ci.*", "ci.failures", true; "star_matches_one_segment")]
    #[test_case("ci.*", "ci", false; "star_needs_a_segment")]
    #[test_case("ci.*", "ci.a.b", false; "star_matches_only_one_segment")]
    #[test_case("*.failures", "ci.failures", true; "leading_star")]
    #[test_case("ci.**", "ci.a", true; "tail_matches_one_segment")]
    #[test_case("ci.**", "ci.a.b", true; "tail_matches_several_segments")]
    #[test_case("ci.**", "ci", false; "tail_needs_a_segment")]
    #[test_case("**", "ci", true; "every_topic")]
    #[test_case("ci.failures", "ci.failures", true; "exact")]
    #[test_case("ci.failures", "ci.failure", false; "exact_mismatch")]
    #[test_case("ci", "ci.failures", false; "exact_is_not_a_prefix")]
    fn patterns_match_topics(pattern: &str, topic: &str, expected: bool) {
        assert_eq!(pattern_matches(pattern, topic), expected);
    }

    #[test]
    fn added_patterns_keep_order_without_duplicates() {
        let current = vec![FAILURES.to_owned()];
        let added = [EVERY_CI_TOPIC.to_owned(), FAILURES.to_owned()];
        assert_eq!(
            add_patterns(&current, &added).unwrap(),
            [FAILURES, EVERY_CI_TOPIC]
        );
    }

    #[test_case(numbered(MAX_PATTERNS), &["ci"], Some(TOO_MANY_PATTERNS); "over_the_limit")]
    #[test_case(numbered(MAX_PATTERNS), &["topic-0"], None; "duplicate_at_the_limit")]
    #[test_case(Vec::new(), &["ci.fail*"], Some(INVALID_PATTERN); "invalid_pattern")]
    fn adding_patterns_is_validated(current: Vec<String>, added: &[&str], error: Option<&str>) {
        let added: Vec<String> = added.iter().copied().map(str::to_owned).collect();
        assert_eq!(add_patterns(&current, &added).err().as_deref(), error);
    }

    #[test_case(&[FAILURES, EVERY_CI_TOPIC], &[FAILURES], Ok(&[EVERY_CI_TOPIC]); "removes_listed_patterns")]
    #[test_case(&[EVERY_CI_TOPIC], &[FAILURES], Err(FAILURES); "refuses_an_unsubscribed_pattern")]
    fn removing_patterns_names_one_it_lacks(
        current: &[&str],
        removed: &[&str],
        expected: Result<&[&str], &str>,
    ) {
        let owned = |values: &[&str]| -> Vec<String> {
            values.iter().copied().map(str::to_owned).collect()
        };
        assert_eq!(
            remove_patterns(&owned(current), &owned(removed)),
            expected
                .map(owned)
                .map_err(|missing| format!("{MISSING_PATTERN}: {missing:?}"))
        );
    }

    #[test_case(numbered(MAX_PATTERNS), None; "at_the_limit")]
    #[test_case(numbered(MAX_PATTERNS + 1), Some(TOO_MANY_PATTERNS); "over_the_limit")]
    #[test_case(vec!["CI".to_owned()], Some(INVALID_PATTERN); "invalid_pattern")]
    fn subscription_sets_are_validated(patterns: Vec<String>, error: Option<&str>) {
        assert_eq!(validate_patterns(&patterns).err().as_deref(), error);
    }
}
