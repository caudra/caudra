//! The JavaScript behaviours upstream leans on without saying so: `localeCompare` ordering,
//! `trim`, `Date#toISOString`, `path.resolve`, `randomUUID`, and how an object orders its keys.

use std::cmp::Ordering;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use jiff::Timestamp;

/// ICU's root collation order for printable ASCII, with case folded away.
const COLLATION_ORDER: &str =
    " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";
const ISO_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";
const UUID_VERSION_MASK: u128 = 0xffff_ffff_ffff_0fff_3fff_ffff_ffff_ffff;
const UUID_VERSION_BITS: u128 = 0x0000_0000_0000_4000_8000_0000_0000_0000;
/// The largest index a JavaScript array, and so an integer-like object key, can hold.
const MAX_ARRAY_INDEX: u64 = u32::MAX as u64 - 1;
pub const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

static UUID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `a.localeCompare(b)` under ICU's root collation, close enough for the ASCII state, verdict,
/// and file names orch sorts: punctuation before digits before letters, case ignored until a
/// tie, then lowercase first.
pub fn locale_compare(left: &str, right: &str) -> Ordering {
    let primary = |text: &str| -> Vec<(bool, u32)> {
        text.chars()
            .map(|character| {
                let folded = character.to_ascii_lowercase();
                match COLLATION_ORDER.find(folded) {
                    Some(rank) => (false, rank as u32),
                    None => (true, u32::from(character)),
                }
            })
            .collect()
    };
    let tertiary = |text: &str| -> Vec<bool> { text.chars().map(char::is_uppercase).collect() };
    primary(left)
        .cmp(&primary(right))
        .then_with(|| tertiary(left).cmp(&tertiary(right)))
        .then_with(|| left.cmp(right))
}

/// Orders keys the way a JavaScript object built from them iterates: integer-like keys first,
/// ascending, then the rest in the order given.
pub fn object_key_order<T>(entries: &mut [(String, T)]) {
    entries.sort_by_key(|(key, _)| array_index(key).map_or((1, 0), |index| (0, index)));
}

fn array_index(key: &str) -> Option<u64> {
    let canonical = key == "0" || (!key.starts_with('0') && !key.is_empty());
    if !canonical || !key.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    key.parse().ok().filter(|index| *index <= MAX_ARRAY_INDEX)
}

fn is_js_whitespace(character: char) -> bool {
    (character.is_whitespace() && character != '\u{85}') || character == '\u{feff}'
}

pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

pub fn iso_timestamp(now: Timestamp) -> String {
    Timestamp::from_millisecond(now.as_millisecond())
        .unwrap_or(now)
        .strftime(ISO_FORMAT)
        .to_string()
}

/// What `path.resolve` makes of an absolute path: `.` and `..` folded lexically.
pub fn normalize(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved
}

/// A version 4 UUID string. Uniqueness is what callers need, since every file it names is
/// created exclusively.
pub fn random_uuid() -> String {
    let counter = UUID_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let high = u128::from(RandomState::new().hash_one(counter));
    let low = u128::from(RandomState::new().hash_one((counter, std::process::id())));
    let value = ((high << 64) | low) & UUID_VERSION_MASK | UUID_VERSION_BITS;
    let hex = format!("{value:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A JSON number as `Number.isSafeInteger` accepts it, which counts `2.0` as an integer.
pub fn safe_integer(value: &serde_json::Value) -> Option<i64> {
    let number = value.as_number()?;
    let integer = match number.as_i64() {
        Some(integer) => integer,
        None => {
            let float = number.as_f64()?;
            if float.fract() != 0.0 || float.abs() > MAX_SAFE_INTEGER as f64 {
                return None;
            }
            float as i64
        }
    };
    (integer.abs() <= MAX_SAFE_INTEGER).then_some(integer)
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::path::{Path, PathBuf};

    use jiff::Timestamp;
    use test_case::test_case;

    use super::{iso_timestamp, locale_compare, normalize, object_key_order, random_uuid};

    #[test]
    fn locale_compare_sorts_like_icu() {
        let mut words = vec![
            "B", "ab", "a_b", "10", "A", "aB", "a-b", "Ab", "9", "a b", "a1", "b", "a", "1",
        ];
        words.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(
            words,
            [
                "1", "10", "9", "a", "A", "a b", "a_b", "a-b", "a1", "ab", "aB", "Ab", "b", "B"
            ]
        );
        assert_eq!(locale_compare("same", "same"), Ordering::Equal);
    }

    #[test]
    fn object_key_order_puts_array_indexes_first() {
        let mut entries: Vec<(String, ())> = ["b", "2", "a", "10", "01"]
            .into_iter()
            .map(|key| (key.to_owned(), ()))
            .collect();
        object_key_order(&mut entries);
        let keys: Vec<&str> = entries.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["2", "10", "b", "a", "01"]);
    }

    #[test_case(0, "1970-01-01T00:00:00.000Z" ; "epoch")]
    #[test_case(1_792_000_123_456_789, "2026-10-14T17:48:43.456Z" ; "truncates_to_milliseconds")]
    fn iso_timestamp_matches_to_iso_string(microseconds: i64, expected: &str) {
        let now = Timestamp::from_microsecond(microseconds).expect("in range");
        assert_eq!(iso_timestamp(now), expected);
    }

    #[test_case("/work/store", "/work/store" ; "plain")]
    #[test_case("/abs/./x/../y/", "/abs/y" ; "dots_and_trailing_slash")]
    #[test_case("/work/a/../b", "/work/b" ; "parent")]
    fn normalize_folds_dots(path: &str, expected: &str) {
        assert_eq!(normalize(Path::new(path)), PathBuf::from(expected));
    }

    #[test]
    fn random_uuid_is_a_distinct_v4() {
        let first = random_uuid();
        assert_eq!(first.len(), 36);
        assert_eq!(&first[14..15], "4");
        assert_ne!(first, random_uuid());
    }
}
