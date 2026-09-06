use std::sync::LazyLock;

use tiktoken_rs::CoreBPE;

static O200K: LazyLock<&'static CoreBPE> = LazyLock::new(tiktoken_rs::o200k_base_singleton);

/// Offline token count for `text`, using OpenAI's o200k_base encoding.
///
/// Exact for the GPT-4o/5 family. Caudra is provider neutral and no other
/// vendor publishes a tokenizer, so treat every count as an estimate and label
/// it as one; it still beats a bytes-per-token heuristic, which collapses on
/// CJK, base64, and dense JSON.
pub fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(O200K.count_ordinary(text)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::estimate_tokens;

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
}
