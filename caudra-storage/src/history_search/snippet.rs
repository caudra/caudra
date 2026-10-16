//! Snippets for the hits a page returns. The index is contentless, so each
//! hit's body is extracted again from its payload and matched in a scratch
//! in-memory FTS5 table with the same tokenizer, which marks exactly the
//! tokens the index matched, stems and folded accents included.

use std::ops::Range;

use rusqlite::{Connection, OptionalExtension, params};

use super::{Snippet, TOKENIZER};
use crate::sessions::SessionError;

pub(super) const SNIPPET_BYTES: usize = 320;
/// Context kept before the first highlighted term of a window.
const SNIPPET_LEAD_BYTES: usize = 64;
const MARK_OPEN: char = '\u{1}';
const MARK_CLOSE: char = '\u{2}';
const BLANK: &str = " ";

pub(super) struct Highlighter {
    connection: Connection,
}

impl Highlighter {
    pub(super) fn new() -> Result<Self, SessionError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(&format!(
            "CREATE VIRTUAL TABLE scratch USING fts5(body, tokenize='{TOKENIZER}');"
        ))?;
        Ok(Self { connection })
    }

    /// The byte ranges in `body` of the tokens `expression` matches.
    pub(super) fn ranges(
        &self,
        body: &str,
        expression: &str,
    ) -> Result<Vec<Range<usize>>, SessionError> {
        let clean = body.replace([MARK_OPEN, MARK_CLOSE], BLANK);
        self.connection.execute("DELETE FROM scratch", [])?;
        self.connection
            .execute("INSERT INTO scratch (rowid, body) VALUES (1, ?1)", [&clean])?;
        let marked: Option<String> = self
            .connection
            .query_row(
                "SELECT highlight(scratch, 0, ?2, ?3) FROM scratch WHERE scratch MATCH ?1",
                params![expression, MARK_OPEN.to_string(), MARK_CLOSE.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let mut ranges = Vec::new();
        let (mut position, mut start) = (0, 0);
        for character in marked.unwrap_or_default().chars() {
            match character {
                MARK_OPEN => start = position,
                MARK_CLOSE => ranges.push(start..position),
                _ => position += character.len_utf8(),
            }
        }
        Ok(ranges)
    }
}

/// At most [`SNIPPET_BYTES`] of `body` around its densest cluster of
/// `ranges`, which must be sorted, with the highlights it holds.
pub(super) fn window(body: &str, ranges: &[Range<usize>]) -> Snippet {
    if body.len() <= SNIPPET_BYTES {
        return Snippet {
            text: body.to_owned(),
            highlights: ranges.to_vec(),
        };
    }
    let reach = SNIPPET_BYTES - SNIPPET_LEAD_BYTES;
    let (mut best, mut best_count, mut last) = (None, 0, 0);
    for (first, range) in ranges.iter().enumerate() {
        last = last.max(first);
        while last < ranges.len() && ranges[last].end <= range.start + reach {
            last += 1;
        }
        if last - first > best_count {
            (best, best_count) = (Some(range.start), last - first);
        }
    }
    let start = best.map_or(0, |start| {
        start
            .saturating_sub(SNIPPET_LEAD_BYTES)
            .min(body.len() - SNIPPET_BYTES)
    });
    let start = body.floor_char_boundary(start);
    let end = body.floor_char_boundary(start + SNIPPET_BYTES);
    Snippet {
        text: body[start..end].to_owned(),
        highlights: ranges
            .iter()
            .filter(|range| range.start >= start && range.end <= end)
            .map(|range| range.start - start..range.end - start)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const STEMMED: &str = "The tests were running slowly, so we ran them again.";
    const ACCENTED: &str = "Le café était fermé.";
    const FILLER: &str = "lorem ipsum dolor sit amet ";
    const TERM: &str = "retry";

    #[test_case(STEMMED, "\"run\"", &["running"]; "porter_stems")]
    #[test_case(ACCENTED, "\"cafe\"", &["café"]; "folded_accents")]
    #[test_case(STEMMED, "\"tests\" OR \"slowly\"", &["tests", "slowly"]; "every_term")]
    #[test_case(STEMMED, "\"absent\"", &[]; "no_match")]
    #[test_case("a\u{1}b retry", "\"retry\"", &["retry"]; "marker_bytes_in_body")]
    fn highlights_mark_the_tokens_the_index_matched(
        body: &str,
        expression: &str,
        expected: &[&str],
    ) {
        let ranges = Highlighter::new()
            .unwrap()
            .ranges(body, expression)
            .unwrap();
        let marked: Vec<&str> = ranges.iter().map(|range| &body[range.clone()]).collect();
        assert_eq!(marked, expected);
    }

    #[test]
    fn window_centres_on_the_densest_cluster() {
        let lone = format!("{TERM} {}", FILLER.repeat(40));
        let cluster = format!("{TERM} {TERM} {TERM}");
        let body = format!("é{lone}{cluster} {}", FILLER.repeat(40));
        let ranges = Highlighter::new()
            .unwrap()
            .ranges(&body, &format!("\"{TERM}\""))
            .unwrap();

        let snippet = window(&body, &ranges);

        assert!(snippet.text.len() <= SNIPPET_BYTES);
        assert!(snippet.text.contains(&cluster));
        assert_eq!(snippet.highlights.len(), 3);
        for range in &snippet.highlights {
            assert_eq!(&snippet.text[range.clone()], TERM);
        }
    }

    #[test]
    fn window_without_highlights_keeps_the_head_on_a_char_boundary() {
        let body = "é".repeat(SNIPPET_BYTES);
        let snippet = window(&body, &[]);
        assert!(body.starts_with(&snippet.text));
        assert!(snippet.text.len() <= SNIPPET_BYTES);
        assert!(snippet.highlights.is_empty());
    }
}
