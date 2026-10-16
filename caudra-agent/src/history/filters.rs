//! Filters typed into a search box as `key:value` tokens, the way code hosts take them:
//! `kind:user`, `tool:shell`, `since:7d`, `until:2026-10-01`. A token whose value does not parse
//! stays a search term, as does anything inside a `"quoted phrase"`.

use caudra_storage::history_search::{HistoryKind, HistoryQuery};
use jiff::tz::TimeZone;
use jiff::{Timestamp, civil};

const KIND_KEY: &str = "kind";
const TOOL_KEY: &str = "tool";
const SINCE_KEY: &str = "since";
const UNTIL_KEY: &str = "until";
const KEY_SEPARATOR: char = ':';
const VALUE_SEPARATORS: [char; 2] = [',', '|'];
const PHRASE_QUOTE: char = '"';
const MILLIS_PER_HOUR: u64 = 3_600_000;
const RELATIVE_UNITS: [(char, u64); 3] = [
    ('h', MILLIS_PER_HOUR),
    ('d', 24 * MILLIS_PER_HOUR),
    ('w', 7 * 24 * MILLIS_PER_HOUR),
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryFilters {
    pub kinds: Option<Vec<HistoryKind>>,
    pub tools: Option<Vec<String>>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
}

impl QueryFilters {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn apply(self, query: &mut HistoryQuery) {
        query.kinds = self.kinds;
        query.tools = self.tools;
        query.since_ms = self.since_ms;
        query.until_ms = self.until_ms;
    }
}

/// Splits `text` into its search terms and its filters. Relative times count back from `now_ms`.
pub fn parse_filters(text: &str, now_ms: u64) -> (String, QueryFilters) {
    let mut filters = QueryFilters::default();
    let mut terms = Vec::new();
    let mut in_phrase = false;
    for word in text.split_whitespace() {
        let taken = !in_phrase && take(word, now_ms, &mut filters);
        if word.matches(PHRASE_QUOTE).count() % 2 == 1 {
            in_phrase = !in_phrase;
        }
        if !taken {
            terms.push(word);
        }
    }
    (terms.join(" "), filters)
}

fn take(word: &str, now_ms: u64, filters: &mut QueryFilters) -> bool {
    let Some((key, value)) = word.split_once(KEY_SEPARATOR) else {
        return false;
    };
    if value.is_empty() {
        return false;
    }
    let values = || {
        value
            .split(VALUE_SEPARATORS)
            .filter(|part| !part.is_empty())
    };
    match key {
        KIND_KEY => values()
            .map(|kind| kind.parse::<HistoryKind>().ok())
            .collect::<Option<Vec<_>>>()
            .map(|kinds| extend(&mut filters.kinds, kinds))
            .is_some(),
        TOOL_KEY => {
            extend(&mut filters.tools, values().map(str::to_owned).collect());
            true
        }
        SINCE_KEY => parse_time(value, now_ms)
            .map(|at| filters.since_ms = Some(at))
            .is_some(),
        UNTIL_KEY => parse_time(value, now_ms)
            .map(|at| filters.until_ms = Some(at))
            .is_some(),
        _ => false,
    }
}

fn extend<T>(slot: &mut Option<Vec<T>>, values: Vec<T>) {
    slot.get_or_insert_with(Vec::new).extend(values);
}

/// `7d`, `12h` or `2w` back from `now_ms`, or an absolute time as [`parse_iso_time`] reads it.
fn parse_time(written: &str, now_ms: u64) -> Option<u64> {
    relative(written)
        .map(|ago| now_ms.saturating_sub(ago))
        .or_else(|| parse_iso_time(written))
}

fn relative(written: &str) -> Option<u64> {
    let unit = written.chars().last()?;
    let (_, millis) = RELATIVE_UNITS.iter().find(|(name, _)| *name == unit)?;
    let count: u64 = written[..written.len() - unit.len_utf8()].parse().ok()?;
    count.checked_mul(*millis)
}

/// An ISO date or datetime as Unix milliseconds. One without an offset is UTC.
pub fn parse_iso_time(written: &str) -> Option<u64> {
    written
        .parse::<Timestamp>()
        .ok()
        .or_else(|| {
            written
                .parse::<civil::DateTime>()
                .ok()
                .and_then(|datetime| datetime.to_zoned(TimeZone::UTC).ok())
                .map(|zoned| zoned.timestamp())
        })
        .and_then(|timestamp| u64::try_from(timestamp.as_millisecond()).ok())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const NOW_MS: u64 = 1_767_225_600_000;
    const DAY_MS: u64 = 24 * MILLIS_PER_HOUR;

    #[test_case("retry loop", "retry loop", QueryFilters::default() ; "plain_terms")]
    #[test_case("kind:user retry", "retry", QueryFilters { kinds: Some(vec![HistoryKind::User]), ..QueryFilters::default() } ; "kind")]
    #[test_case("kind:user,tool_call x", "x", QueryFilters { kinds: Some(vec![HistoryKind::User, HistoryKind::ToolCall]), ..QueryFilters::default() } ; "several_kinds")]
    #[test_case("kind:bogus x", "kind:bogus x", QueryFilters::default() ; "unknown_kind_stays_a_term")]
    #[test_case("tool:shell tool:code x", "x", QueryFilters { tools: Some(vec!["shell".into(), "code".into()]), ..QueryFilters::default() } ; "tools_accumulate")]
    #[test_case("since:7d x", "x", QueryFilters { since_ms: Some(NOW_MS - 7 * DAY_MS), ..QueryFilters::default() } ; "relative_days")]
    #[test_case("since:12h", "", QueryFilters { since_ms: Some(NOW_MS - 12 * MILLIS_PER_HOUR), ..QueryFilters::default() } ; "relative_hours_alone")]
    #[test_case("until:2026-01-01 x", "x", QueryFilters { until_ms: Some(NOW_MS), ..QueryFilters::default() } ; "absolute_date")]
    #[test_case("since:yesterday x", "since:yesterday x", QueryFilters::default() ; "bad_time_stays_a_term")]
    #[test_case("foo:bar x", "foo:bar x", QueryFilters::default() ; "unknown_key_stays_a_term")]
    #[test_case("tool: x", "tool: x", QueryFilters::default() ; "empty_value_stays_a_term")]
    #[test_case("\"see kind:user here\" kind:assistant", "\"see kind:user here\"", QueryFilters { kinds: Some(vec![HistoryKind::Assistant]), ..QueryFilters::default() } ; "phrases_keep_their_tokens")]
    fn filters_come_out_of_the_query(text: &str, terms: &str, filters: QueryFilters) {
        assert_eq!(parse_filters(text, NOW_MS), (terms.to_owned(), filters));
    }
}
