use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{LazyLock, Mutex};

use tiktoken_rs::{CoreBPE, Rank};

use crate::model::{format_tokens, format_tokens_wide};

/// Cleared wholesale on overflow rather than evicted one by one. Tool results
/// are stable within a turn, so the worst a clear costs is re-counting them
/// once; tracking recency to avoid that would cost more than it saves.
const CACHE_CAPACITY: usize = 4096;
/// Marks a count as an estimate: o200k is exact only for OpenAI models.
const TOKEN_ESTIMATE_MARKER: &str = "~";
const TOKEN_SUFFIX: &str = " tokens";
/// A rate with nothing to score, told apart from a rate of zero.
const NO_RATE: &str = "—";
const PERCENT: f64 = 100.0;

static O200K: LazyLock<&'static CoreBPE> = LazyLock::new(tiktoken_rs::o200k_base_singleton);

static CACHE: LazyLock<Mutex<HashMap<u64, u32>>> = LazyLock::new(Mutex::default);

/// Offline token count for `text`, using OpenAI's o200k_base encoding.
///
/// Exact for the GPT-4o/5 family. Caudra is provider neutral and no other
/// vendor publishes a tokenizer, so treat every count as an estimate and label
/// it as one; it still beats a bytes-per-token heuristic, which collapses on
/// CJK, base64, and dense JSON.
pub fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(O200K.count_ordinary(text)).unwrap_or(u32::MAX)
}

/// A text cut to its first `head` and last `tail` tokens, with the count that
/// fell out between them.
#[derive(Debug, PartialEq, Eq)]
pub struct MiddleCut {
    pub head: String,
    pub tail: String,
    pub omitted: u32,
}

/// Cuts the middle out of `text` so its beginning and end survive: `None` when
/// the whole text already fits in `head + tail` tokens.
///
/// A token boundary can fall inside a multibyte character, so each side is
/// decoded lossily and the replacement glyph that marks the split is trimmed
/// off rather than handed on.
pub fn cut_middle(text: &str, head: u32, tail: u32) -> Option<MiddleCut> {
    let tokens = O200K.encode_ordinary(text);
    let (head, tail) = (head as usize, tail as usize);
    if tokens.len() <= head + tail {
        return None;
    }
    let decode = |slice: &[Rank]| {
        O200K
            .decode_bytes(slice)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let head_text = decode(&tokens[..head]).ok()?;
    let tail_text = decode(&tokens[tokens.len() - tail..]).ok()?;
    Some(MiddleCut {
        head: head_text
            .trim_end_matches(char::REPLACEMENT_CHARACTER)
            .trim_end()
            .to_owned(),
        tail: tail_text
            .trim_start_matches(char::REPLACEMENT_CHARACTER)
            .trim_start()
            .to_owned(),
        omitted: u32::try_from(tokens.len() - head - tail).unwrap_or(u32::MAX),
    })
}

/// [`estimate_tokens`], memoized on a hash of `text`.
///
/// For callers that re-count the same immutable content every turn. Counting
/// runs at 7-13 MB/s, so a site that walks the whole history per request pays
/// tens of milliseconds before the first streamed token without this.
///
/// Keyed on a 64-bit hash rather than the text, so a collision would return
/// another string's count. At ~1e-13 for a full cache that is well below the
/// error already carried by counting a non-OpenAI model with o200k; do not
/// reuse this where a wrong count would be a correctness bug rather than a
/// worse estimate.
pub fn estimate_tokens_cached(text: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let key = hasher.finish();

    // A poisoned cache is not worth failing a request over; it only ever holds
    // derived data, so fall back to counting.
    let Ok(mut cache) = CACHE.lock() else {
        return estimate_tokens(text);
    };
    if let Some(count) = cache.get(&key) {
        return *count;
    }
    let count = estimate_tokens(text);
    if cache.len() >= CACHE_CAPACITY {
        cache.clear();
    }
    cache.insert(key, count);
    count
}

/// Renders a count for display. The marker is not decoration: every count this
/// module produces is an estimate for any model outside the GPT-4o/5 family.
pub fn token_label(tokens: u32) -> String {
    format!(
        "{TOKEN_ESTIMATE_MARKER}{}{TOKEN_SUFFIX}",
        format_tokens(tokens)
    )
}

/// [`format_tokens`] takes the `u32` a session counts in; a lifetime total
/// needs the wider one.
pub fn format_tokens_u64(value: u64) -> String {
    format_tokens_wide(value)
}

/// Renders a cache hit rate for a table column. `None` is a provider that
/// reported no prompt tokens to score, which every surface must show as
/// unknown rather than as a 0% hit.
pub fn format_hit_rate(rate: Option<f64>) -> String {
    rate.map_or_else(
        || NO_RATE.to_owned(),
        |rate| format!("{:.0}%", rate * PERCENT),
    )
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        CACHE_CAPACITY, NO_RATE, cut_middle, estimate_tokens, estimate_tokens_cached,
        format_hit_rate, format_tokens_u64,
    };

    const HEAD: u32 = 8;
    const TAIL: u32 = 12;

    #[test_case(None, NO_RATE ; "nothing_to_score_is_not_a_zero_hit")]
    #[test_case(Some(0.0), "0%" ; "a_real_zero_is_a_number")]
    #[test_case(Some(0.925), "92%" ; "rounds_to_whole_percent")]
    #[test_case(Some(1.0), "100%" ; "wholly_cached")]
    fn hit_rates_tell_unknown_apart_from_zero(rate: Option<f64>, expected: &str) {
        assert_eq!(format_hit_rate(rate), expected);
    }

    #[test_case(u64::from(u32::MAX), "4295m" ; "largest_session_count")]
    #[test_case(u64::from(u32::MAX) + 1, "4295m" ; "past_session_count")]
    #[test_case(10_000_000_000, "10000m" ; "lifetime_total_is_not_capped")]
    #[test_case(u64::MAX, "18446744073709.6m" ; "largest_lifetime_total")]
    fn wide_token_counts_keep_their_value(value: u64, expected: &str) {
        assert_eq!(format_tokens_u64(value), expected);
    }

    #[test_case("", 0 ; "empty_text_costs_nothing")]
    #[test_case("hello", 1 ; "a_common_word_is_one_token")]
    #[test_case("The quick brown fox jumps over the lazy dog.", 10 ; "prose_splits_on_words")]
    fn estimate_tokens_matches_o200k(text: &str, expected: u32) {
        assert_eq!(estimate_tokens(text), expected);
    }

    /// The reason for a real tokenizer over `len() / 4`: text with no ASCII
    /// word boundaries costs far more than its byte count suggests.
    #[test]
    fn dense_text_costs_more_than_a_byte_heuristic_predicts() {
        let cjk = "\u{6f22}\u{5b57}".repeat(64);
        assert!(estimate_tokens(&cjk) > u32::try_from(cjk.len()).unwrap() / 4);
    }

    #[test_case("" ; "empty_text")]
    #[test_case("hello world" ; "short_prose")]
    #[test_case("{\"a\":1,\"b\":[2,3]}" ; "dense_json")]
    fn the_cache_agrees_with_a_direct_count(text: &str) {
        assert_eq!(estimate_tokens_cached(text), estimate_tokens(text));
        assert_eq!(
            estimate_tokens_cached(text),
            estimate_tokens(text),
            "a second call must hit the cache and still agree"
        );
    }

    #[test_case("" ; "empty_text")]
    #[test_case("short enough to keep whole" ; "under_the_cut")]
    fn a_text_that_fits_is_not_cut(text: &str) {
        assert_eq!(cut_middle(text, HEAD, TAIL), None);
    }

    #[test]
    fn a_long_text_keeps_its_ends_within_the_token_limits() {
        let text = (1..=60)
            .map(|n| format!("sentence number {n} of the reply."))
            .collect::<Vec<_>>()
            .join(" ");
        let cut = cut_middle(&text, HEAD, TAIL).expect("sixty sentences exceed twenty tokens");
        assert!(text.starts_with(&cut.head), "the head opens the text");
        assert!(text.ends_with(&cut.tail), "the tail closes the text");
        assert!(estimate_tokens(&cut.head) <= HEAD);
        assert!(estimate_tokens(&cut.tail) <= TAIL);
        assert_eq!(cut.omitted, estimate_tokens(&text) - HEAD - TAIL);
    }

    /// A token boundary inside a multibyte character must not leak the
    /// replacement glyph into either side.
    #[test]
    fn a_split_multibyte_character_is_trimmed_not_replaced() {
        let cjk = "\u{6f22}\u{5b57}\u{3042}".repeat(64);
        let cut = cut_middle(&cjk, HEAD, TAIL).expect("dense text exceeds the cut");
        assert!(!cut.head.contains(char::REPLACEMENT_CHARACTER));
        assert!(!cut.tail.contains(char::REPLACEMENT_CHARACTER));
        assert!(cjk.starts_with(&cut.head));
        assert!(cjk.ends_with(&cut.tail));
    }

    /// The cache clears rather than evicting, so the entry that triggered the
    /// clear must still be readable afterwards.
    #[test]
    fn the_cache_survives_passing_its_capacity() {
        for i in 0..CACHE_CAPACITY + 16 {
            let text = format!("entry number {i}");
            assert_eq!(estimate_tokens_cached(&text), estimate_tokens(&text));
        }
    }
}
