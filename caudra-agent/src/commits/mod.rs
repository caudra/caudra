//! `#hash` commit mentions.
//!
//! A commit mention names one revision of the project's repository inside prose
//! the user typed: `#a1b2c3d`. The composer's popup inserts the spelling, and
//! the workbench emits the same one when it sends a commit to the composer.
//!
//! Parsing alone cannot tell a hash from a heading, an issue number or a colour
//! literal, so [`scan`] pairs the grammar with a resolution predicate. What
//! satisfies that predicate is deliberately narrow: membership in the log window
//! the composer already walked, never a fresh probe of the repository. A
//! keystroke must not open an object database, and a revision nobody listed is
//! not one the reader picked.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::sigil::{MAX_CANDIDATES, TRAILING_PUNCTUATION, opens_after};

pub mod repo;

const SIGIL: char = '#';
/// Git's own abbreviation floor. Six hexadecimal digits is a CSS colour and
/// three is a heading anchor, so the shortest thing worth resolving is seven.
const MIN_LENGTH: usize = 7;
const MAX_LENGTH: usize = 40;

/// A reference to a commit, with the source text that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRef {
    /// The mention exactly as it appears in the composer, sigil included. Kept
    /// verbatim so a restored draft renders the text the user actually typed.
    pub raw: String,
    /// The hexadecimal revision, lowercased. Abbreviated or full, as typed.
    pub id: String,
}

impl CommitRef {
    /// Builds a reference in canonical spelling, for insertions the UI originates.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into().to_ascii_lowercase();
        Self {
            raw: format(&id),
            id,
        }
    }
}

/// Renders `id` in the spelling [`scan`] accepts.
pub fn format(id: &str) -> String {
    let mut out = String::with_capacity(id.len() + SIGIL.len_utf8());
    out.push(SIGIL);
    out.push_str(id);
    out
}

/// Finds every commit mention in `text` whose revision satisfies `resolves`.
///
/// Ranges are char offsets, matching the composer's span model. Mentions never
/// overlap: a match consumes its own text before scanning resumes.
pub fn scan(text: &str, mut resolves: impl FnMut(&str) -> bool) -> Vec<(Range<usize>, CommitRef)> {
    let mut found = Vec::new();
    let mut boundary = true;
    let mut candidates = 0;
    let mut cursor = text.char_indices().enumerate();
    while let Some((char_index, (byte_index, character))) = cursor.next() {
        if character == SIGIL && boundary && candidates < MAX_CANDIDATES {
            candidates += 1;
            if let Some((end, id)) = parse_from(text, byte_index)
                && resolves(&id)
            {
                let raw = text[byte_index..end].to_owned();
                let width = raw.chars().count();
                found.push((char_index..char_index + width, CommitRef { raw, id }));
                for _ in 1..width {
                    cursor.next();
                }
                boundary = false;
                continue;
            }
        }
        boundary = opens_after(character);
    }
    found
}

/// Parses one mention starting at `at`, a byte offset that must land on the
/// sigil. Returns the byte offset just past the mention and the lowercased id.
///
/// The run is taken to the next whitespace before it is measured, so a hash
/// that merely prefixes a longer word is rejected rather than truncated to fit.
fn parse_from(text: &str, at: usize) -> Option<(usize, String)> {
    let body = text.get(at..)?.strip_prefix(SIGIL)?;
    let end = body.find(char::is_whitespace).unwrap_or(body.len());
    let run = body[..end].trim_end_matches(TRAILING_PUNCTUATION);
    if !(MIN_LENGTH..=MAX_LENGTH).contains(&run.len())
        || !run.chars().all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    Some((at + SIGIL.len_utf8() + run.len(), run.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SHORT: &str = "a1b2c3d";
    const FULL: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const EXPECT_SCANNED: &str = "the text names exactly one commit";
    const EXPECT_PROSE: &str = "the text names no commit";

    /// Resolves anything the grammar admits, so these cases measure the grammar
    /// alone rather than the log window behind it.
    fn any(text: &str) -> Vec<(Range<usize>, CommitRef)> {
        scan(text, |_| true)
    }

    #[test_case("#a1b2c3d", Some("a1b2c3d") ; "bare_short_hash")]
    #[test_case("see #a1b2c3d", Some("a1b2c3d") ; "after_whitespace")]
    #[test_case("(#a1b2c3d)", Some("a1b2c3d") ; "inside_delimiters")]
    #[test_case("landed in #a1b2c3d.", Some("a1b2c3d") ; "trailing_period")]
    #[test_case("#A1B2C3D", Some("a1b2c3d") ; "uppercase_is_normalized")]
    #[test_case("# Heading", None ; "markdown_heading")]
    #[test_case("#123", None ; "issue_number_is_too_short")]
    #[test_case("#a1b2c3", None ; "css_colour_is_too_short")]
    #[test_case("#a1b2c3dZZZ", None ; "hash_prefixing_a_word")]
    #[test_case("issue#a1b2c3d", None ; "mid_word_sigil")]
    #[test_case("#", None ; "sigil_alone")]
    fn the_grammar_admits_hashes_and_leaves_prose_alone(text: &str, expected: Option<&str>) {
        let found = any(text);
        match expected {
            Some(id) => {
                assert_eq!(found.len(), 1, "{EXPECT_SCANNED}: {text}");
                assert_eq!(found[0].1.id, id, "{text}");
            }
            None => assert!(found.is_empty(), "{EXPECT_PROSE}: {text}"),
        }
    }

    #[test_case(MAX_LENGTH, true ; "full_hash")]
    #[test_case(MAX_LENGTH + 1, false ; "past_a_full_hash")]
    #[test_case(MIN_LENGTH, true ; "shortest_accepted")]
    #[test_case(MIN_LENGTH - 1, false ; "one_short_of_accepted")]
    fn length_bounds_are_inclusive(length: usize, expected: bool) {
        let hex: String = FULL.chars().cycle().take(length).collect();
        let text = format(&hex);
        assert_eq!(any(&text).len(), usize::from(expected), "{text}");
    }

    #[test]
    fn a_range_covers_the_sigil_and_the_hash() {
        let found = any("see #a1b2c3d now");
        assert_eq!(found[0].0, 4..4 + 1 + SHORT.len());
        assert_eq!(found[0].1.raw, "#a1b2c3d");
    }

    #[test]
    fn a_predicate_that_declines_leaves_the_text_as_prose() {
        assert!(scan("#a1b2c3d", |_| false).is_empty(), "{EXPECT_PROSE}");
    }

    #[test]
    fn the_predicate_sees_the_normalized_id() {
        let mut seen = Vec::new();
        scan("#A1B2C3D", |id| {
            seen.push(id.to_owned());
            true
        });
        assert_eq!(seen, [SHORT]);
    }

    /// A prompt that is nothing but sigils must not turn one keystroke into an
    /// unbounded run of resolutions.
    #[test]
    fn resolution_stops_at_the_candidate_cap() {
        let text = vec![format(SHORT); MAX_CANDIDATES * 2].join(" ");
        let mut resolved = 0;
        scan(&text, |_| {
            resolved += 1;
            true
        });
        assert_eq!(resolved, MAX_CANDIDATES);
    }

    #[test]
    fn two_mentions_on_one_line_are_found_separately() {
        let found = any("#a1b2c3d and #0f1e2d3");
        assert_eq!(
            found.iter().map(|(_, r)| r.id.as_str()).collect::<Vec<_>>(),
            ["a1b2c3d", "0f1e2d3"]
        );
    }

    #[test]
    fn a_reference_round_trips_through_its_own_spelling() {
        let built = CommitRef::new("A1B2C3D");
        let found = any(&built.raw);
        assert_eq!(found[0].1, built);
    }
}
