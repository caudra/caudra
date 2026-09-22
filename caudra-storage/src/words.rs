//! The word lists behind every name Caudra shows a person instead of a hash.
//!
//! A plan file and a scratch directory are both read aloud, pasted into a
//! prompt and typed at a shell, so both are worth a phrase rather than a digest.
//! They differ only in where the seed comes from. A plan name is new every
//! time and draws it from the system RNG. A scratch directory has to resolve to
//! the same name on every run of the same project, so it derives the seed from
//! a hash of whatever makes that project itself.

use std::sync::LazyLock;

use sha2::{Digest, Sha256};

const SEED_BYTES: usize = 12;

static ADJECTIVES: LazyLock<Vec<&str>> =
    LazyLock::new(|| load_words(include_str!("words/adjectives.txt")));
static NOUNS: LazyLock<Vec<&str>> = LazyLock::new(|| load_words(include_str!("words/nouns.txt")));

/// Every word is lowercase ASCII, which is what lets a phrase be joined into a
/// path component and interpolated into a remote shell command without quoting:
/// it can then hold no separator, no metacharacter and no leading dash. The
/// lists are compiled in, so a violation is a broken build rather than bad
/// input, and the first read is the honest place to say so.
fn load_words(text: &'static str) -> Vec<&'static str> {
    let words: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    assert!(!words.is_empty(), "word list must not be empty");
    assert!(
        words
            .iter()
            .all(|word| word.chars().all(|c| c.is_ascii_lowercase())),
        "every word must be lowercase ascii"
    );
    words
}

fn index(seed: &[u8; SEED_BYTES], at: usize, len: usize) -> usize {
    u32::from_le_bytes([seed[at], seed[at + 1], seed[at + 2], seed[at + 3]]) as usize % len
}

/// `adjective-adjective-noun`, with the second adjective nudged along when it
/// lands on the first so a phrase never stutters.
fn phrase(seed: &[u8; SEED_BYTES]) -> String {
    let first = index(seed, 0, ADJECTIVES.len());
    let mut second = index(seed, 4, ADJECTIVES.len());
    let noun = index(seed, 8, NOUNS.len());
    if first == second {
        second = (second + 1) % ADJECTIVES.len();
    }
    format!(
        "{}-{}-{}",
        ADJECTIVES[first], ADJECTIVES[second], NOUNS[noun]
    )
}

pub(crate) fn random_phrase() -> String {
    let mut seed = [0u8; SEED_BYTES];
    getrandom::fill(&mut seed).expect("rng failed");
    phrase(&seed)
}

/// The same phrase for the same value, on every run and every machine.
///
/// `domain` keeps two callers that hash the same bytes for different reasons
/// from landing on one name, and its length prefix keeps the split between
/// domain and value from being forged by a value that spells out another
/// domain.
pub(crate) fn derived_phrase(domain: &str, value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_be_bytes());
    hasher.update(domain.as_bytes());
    hasher.update(value);
    let mut seed = [0u8; SEED_BYTES];
    seed.copy_from_slice(&hasher.finalize()[..SEED_BYTES]);
    phrase(&seed)
}

#[cfg(test)]
mod tests {
    use super::{derived_phrase, random_phrase};
    use test_case::test_case;

    const DOMAIN: &str = "words-test.v1";
    const OTHER_DOMAIN: &str = "words-test.v2";
    const VALUE: &[u8] = b"/home/user/app";
    const OTHER_VALUE: &[u8] = b"/home/user/other";
    /// Pinned, not computed. A derived phrase names a directory a running
    /// session was already handed, so drift here moves it out from under one.
    const PINNED: &str = "happy-cute-tick";
    const SHAPE: &str = "a phrase must be three lowercase words joined by hyphens";
    const STUTTER: &str = "a phrase must not repeat its adjective";
    const UNSTABLE: &str = "one domain and value must always give one phrase";
    const SHARED_NAME: &str = "a different input must not reuse the same name";

    #[test]
    fn a_derived_phrase_is_pinned_to_its_input() {
        assert_eq!(derived_phrase(DOMAIN, VALUE), PINNED);
    }

    #[test]
    fn a_derived_phrase_is_stable_and_separated_by_domain_and_value() {
        assert_eq!(
            derived_phrase(DOMAIN, VALUE),
            derived_phrase(DOMAIN, VALUE),
            "{UNSTABLE}"
        );
        assert_ne!(
            derived_phrase(DOMAIN, VALUE),
            derived_phrase(OTHER_DOMAIN, VALUE),
            "{SHARED_NAME}"
        );
        assert_ne!(
            derived_phrase(DOMAIN, VALUE),
            derived_phrase(DOMAIN, OTHER_VALUE),
            "{SHARED_NAME}"
        );
    }

    /// The shape a remote scratch directory relies on: its name reaches a
    /// remote shell unquoted, so nothing in it may read as a separator, an
    /// option or a metacharacter.
    #[test_case(random_phrase() ; "random")]
    #[test_case(derived_phrase(DOMAIN, VALUE) ; "derived")]
    fn a_phrase_is_three_lowercase_words(phrase: String) {
        let words: Vec<&str> = phrase.split('-').collect();
        assert_eq!(words.len(), 3, "{SHAPE}: {phrase}");
        assert!(
            words
                .iter()
                .all(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase())),
            "{SHAPE}: {phrase}"
        );
        assert_ne!(words[0], words[1], "{STUTTER}: {phrase}");
    }
}
