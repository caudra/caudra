//! Parsing for the JSON records `tracing_subscriber`'s `json()` formatter
//! writes, plus the filter both the `/logs` modal and `caudra logs` apply.
//!
//! The shape is fixed by the formatter: `timestamp`, `level`, `target`, a
//! nested `fields` object whose `message` key holds the event message, and
//! optional `span` and `spans` keys once spans exist.

use std::borrow::Cow;
use std::fmt;

use serde_json::Value;

const FIELD_TIMESTAMP: &str = "timestamp";
const FIELD_LEVEL: &str = "level";
const FIELD_TARGET: &str = "target";
const FIELD_FIELDS: &str = "fields";
const FIELD_MESSAGE: &str = "message";
const FIELD_SPANS: &str = "spans";
const FIELD_SPAN: &str = "span";
const FIELD_NAME: &str = "name";
const TIME_START: usize = 11;
const TIME_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl Level {
    pub const ALL: [Self; 5] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.as_bytes().first()? {
            b'T' | b't' => Some(Self::Trace),
            b'D' | b'd' => Some(Self::Debug),
            b'I' | b'i' => Some(Self::Info),
            b'W' | b'w' => Some(Self::Warn),
            b'E' | b'e' => Some(Self::Error),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    /// Wraps around so a single key can cycle the filter in the modal.
    pub fn next(self) -> Self {
        match self {
            Self::Trace => Self::Debug,
            Self::Debug => Self::Info,
            Self::Info => Self::Warn,
            Self::Warn => Self::Error,
            Self::Error => Self::Trace,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A span on the event's ancestry, with the fields it was opened with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub name: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub timestamp: String,
    pub level: Level,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
    pub spans: Vec<Span>,
}

impl Record {
    /// The `HH:MM:SS.mmm` slice of an RFC3339 timestamp, for the one-line view.
    /// Falls back to the whole string when the shape is unexpected.
    pub fn time_of_day(&self) -> &str {
        self.timestamp
            .get(TIME_START..TIME_START + TIME_LEN)
            .unwrap_or(&self.timestamp)
    }
}

/// A line that did not parse still has to render. Panic output, a stray
/// `eprintln!`, or a half-written trailing line all arrive here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Record(Record),
    Raw(String),
}

impl Entry {
    pub fn parse(line: &str) -> Self {
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(map)) => Self::from_object(map, line),
            _ => Self::Raw(line.to_owned()),
        }
    }

    fn from_object(mut map: serde_json::Map<String, Value>, line: &str) -> Self {
        let Some(level) = map
            .get(FIELD_LEVEL)
            .and_then(Value::as_str)
            .and_then(Level::parse)
        else {
            return Self::Raw(line.to_owned());
        };
        let mut fields = Vec::new();
        let mut message = String::new();
        if let Some(Value::Object(inner)) = map.remove(FIELD_FIELDS) {
            for (key, value) in inner {
                if key == FIELD_MESSAGE {
                    message = scalar(&value).into_owned();
                } else {
                    fields.push((key, scalar(&value).into_owned()));
                }
            }
        }
        Self::Record(Record {
            timestamp: take_string(&mut map, FIELD_TIMESTAMP),
            level,
            target: take_string(&mut map, FIELD_TARGET),
            message,
            fields,
            spans: spans(&mut map),
        })
    }

    pub fn level(&self) -> Level {
        match self {
            Self::Record(record) => record.level,
            // A raw line is usually a panic or a crash trace, so it must
            // survive every filter except an explicit level floor above error.
            Self::Raw(_) => Level::Error,
        }
    }
}

fn take_string(map: &mut serde_json::Map<String, Value>, key: &str) -> String {
    match map.remove(key) {
        Some(Value::String(s)) => s,
        Some(other) => scalar(&other).into_owned(),
        None => String::new(),
    }
}

/// `spans` carries the whole ancestry and `span` repeats the innermost one, so
/// preferring `spans` avoids rendering the same span twice.
fn spans(map: &mut serde_json::Map<String, Value>) -> Vec<Span> {
    let raw = match map.remove(FIELD_SPANS) {
        Some(Value::Array(items)) => items,
        _ => match map.remove(FIELD_SPAN) {
            Some(one) => vec![one],
            None => return Vec::new(),
        },
    };
    raw.into_iter()
        .filter_map(|item| match item {
            Value::Object(mut obj) => {
                let name = take_string(&mut obj, FIELD_NAME);
                let fields = obj
                    .into_iter()
                    .map(|(k, v)| (k, scalar(&v).into_owned()))
                    .collect();
                Some(Span { name, fields })
            }
            _ => None,
        })
        .collect()
}

/// Field values render as text. Strings pass through unquoted so a message
/// reads naturally; everything else keeps its JSON form.
fn scalar(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(s) => Cow::Borrowed(s),
        Value::Null => Cow::Borrowed(""),
        other => Cow::Owned(other.to_string()),
    }
}

/// Applied while scanning, so a restrictive filter still fills a viewport
/// instead of showing whatever happened to be in the last chunk.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub min_level: Level,
    /// Whitespace-separated and lowercased once, because a scan can cross fifty
    /// thousand lines and must not redo that work per line.
    terms: Vec<Vec<char>>,
}

impl Filter {
    pub fn new(min_level: Level, query: &str) -> Self {
        Self {
            min_level,
            terms: query
                .split_whitespace()
                .map(|term| term.to_lowercase().chars().collect())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.min_level == Level::Trace && self.terms.is_empty()
    }

    pub fn matches(&self, entry: &Entry) -> bool {
        entry.level() >= self.min_level && self.terms.iter().all(|term| matches_term(entry, term))
    }
}

/// A term is matched against each part on its own. Letting one term run from
/// the target into an unrelated field value would match nearly everything.
fn matches_term(entry: &Entry, term: &[char]) -> bool {
    match entry {
        Entry::Raw(line) => fuzzy(line, term),
        Entry::Record(record) => {
            fuzzy(&record.message, term)
                || fuzzy(&record.target, term)
                || fuzzy(record.level.as_str(), term)
                || record
                    .fields
                    .iter()
                    .any(|(k, v)| fuzzy(k, term) || fuzzy(v, term))
                || record.spans.iter().any(|span| {
                    fuzzy(&span.name, term)
                        || span
                            .fields
                            .iter()
                            .any(|(k, v)| fuzzy(k, term) || fuzzy(v, term))
                })
        }
    }
}

/// Case-insensitive subsequence match, so `tolcal` finds `tool_call`. Records
/// stay in the order they were written, so filtering needs no score and this
/// stays a single pass that allocates nothing.
fn fuzzy(haystack: &str, term: &[char]) -> bool {
    let mut rest = term;
    for c in haystack.chars().flat_map(char::to_lowercase) {
        let Some((next, tail)) = rest.split_first() else {
            return true;
        };
        if c == *next {
            rest = tail;
        }
    }
    rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const EVENT: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"WARN","fields":{"message":"retryable, will retry","attempt":3,"delay_ms":8000},"target":"caudra::provider"}"#;
    const WITH_SPANS: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"INFO","fields":{"message":"done"},"target":"caudra::agent","spans":[{"session_id":"abc","name":"session"},{"name":"turn","turn_id":4}]}"#;
    const NOT_JSON: &str = "thread 'main' panicked at src/main.rs:1:1";
    const JSON_BUT_NOT_A_RECORD: &str = r#"{"hello":"world"}"#;
    const MESSAGE: &str = "retryable, will retry";
    const TARGET: &str = "caudra::provider";
    const TIME_OF_DAY: &str = "14:22:07.418";

    fn record(line: &str) -> Record {
        match Entry::parse(line) {
            Entry::Record(record) => record,
            Entry::Raw(raw) => panic!("expected a record, got raw: {raw}"),
        }
    }

    #[test]
    fn parses_the_formatter_shape() {
        let record = record(EVENT);
        assert_eq!(record.level, Level::Warn);
        assert_eq!(record.target, TARGET);
        assert_eq!(record.message, MESSAGE);
        assert_eq!(record.time_of_day(), TIME_OF_DAY);
    }

    #[test]
    fn message_is_removed_from_the_fields() {
        let record = record(EVENT);
        assert!(record.fields.iter().all(|(k, _)| k != "message"));
        assert!(record.fields.contains(&("attempt".into(), "3".into())));
        assert!(record.fields.contains(&("delay_ms".into(), "8000".into())));
    }

    #[test]
    fn span_ancestry_keeps_its_fields() {
        let record = record(WITH_SPANS);
        let names: Vec<&str> = record.spans.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["session", "turn"]);
        assert_eq!(
            record.spans[0].fields,
            [("session_id".into(), "abc".into())]
        );
        assert_eq!(record.spans[1].fields, [("turn_id".into(), "4".into())]);
    }

    #[test_case(NOT_JSON ; "plain text")]
    #[test_case(JSON_BUT_NOT_A_RECORD ; "json without a level")]
    fn unparseable_lines_survive_as_raw(line: &str) {
        assert_eq!(Entry::parse(line), Entry::Raw(line.to_owned()));
    }

    #[test]
    fn raw_lines_outrank_every_filter_below_error() {
        let filter = Filter::new(Level::Error, "");
        assert!(filter.matches(&Entry::parse(NOT_JSON)));
    }

    #[test_case(Level::Trace, true ; "below")]
    #[test_case(Level::Warn, true ; "equal")]
    #[test_case(Level::Error, false ; "above")]
    fn min_level_filters_by_severity(min_level: Level, expected: bool) {
        let filter = Filter::new(min_level, "");
        assert_eq!(filter.matches(&Entry::parse(EVENT)), expected);
    }

    #[test_case("RETRYABLE", true ; "message case insensitive")]
    #[test_case("provider", true ; "target")]
    #[test_case("delay_ms", true ; "field key")]
    #[test_case("8000", true ; "field value")]
    #[test_case("warn", true ; "level")]
    #[test_case("compaction", false ; "absent")]
    fn a_term_searches_message_target_level_and_fields(query: &str, expected: bool) {
        let filter = Filter::new(Level::Trace, query);
        assert_eq!(filter.matches(&Entry::parse(EVENT)), expected);
    }

    #[test]
    fn a_term_searches_span_fields() {
        assert!(Filter::new(Level::Trace, "abc").matches(&Entry::parse(WITH_SPANS)));
    }

    #[test_case("rtry", true ; "gaps in the message")]
    #[test_case("cdprvdr", true ; "gaps in the target")]
    #[test_case("yrter", false ; "right letters wrong order")]
    fn a_term_matches_as_a_subsequence(query: &str, expected: bool) {
        let filter = Filter::new(Level::Trace, query);
        assert_eq!(filter.matches(&Entry::parse(EVENT)), expected);
    }

    #[test]
    fn every_term_has_to_match_something_and_they_may_match_different_parts() {
        let entry = Entry::parse(EVENT);
        assert!(Filter::new(Level::Trace, "provider retry").matches(&entry));
        assert!(!Filter::new(Level::Trace, "provider compaction").matches(&entry));
    }

    #[test]
    fn a_term_never_runs_from_one_part_into_another() {
        // "provider" ends the target and "retryable" opens the message, so a
        // match here would mean the parts had been concatenated.
        assert!(!Filter::new(Level::Trace, "providerretryable").matches(&Entry::parse(EVENT)));
    }

    #[test]
    fn a_blank_query_is_not_a_filter() {
        assert!(Filter::new(Level::Trace, "   ").is_empty());
    }
}
