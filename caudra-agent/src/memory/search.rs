//! Word search over the current notes: the way in to what the view only
//! names, or has folded into a line about something else.

/// Most hits one search returns.
pub const MAX_HITS: usize = 10;
/// Most bytes of a hit's matching line.
pub const MAX_LINE_BYTES: usize = 200;

const NAME_WEIGHT: usize = 4;
const BODY_WEIGHT: usize = 1;
const ELLIPSIS: &str = "…";

/// A current note as search reads it.
pub struct Document<'a> {
    pub seq: u64,
    pub name: &'a str,
    pub heading: &'a str,
    pub body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub seq: u64,
    pub name: String,
    pub heading: String,
    /// The body's first line holding a term, when the body has one.
    pub line: Option<String>,
}

/// The query's words, lowercased: runs of letters, digits and underscores.
pub fn terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// The notes that best match `terms`, at most [`MAX_HITS`]: a term in a
/// note's name counts more than one in its body, and the more recent note
/// wins a tie.
pub fn search<'a>(terms: &[String], documents: impl IntoIterator<Item = Document<'a>>) -> Vec<Hit> {
    let mut scored: Vec<(usize, Document<'a>)> = documents
        .into_iter()
        .filter_map(|document| {
            let name = document.name.to_lowercase();
            let body = document.body.to_lowercase();
            let score: usize = terms
                .iter()
                .map(|term| {
                    NAME_WEIGHT * usize::from(name.contains(term.as_str()))
                        + BODY_WEIGHT * usize::from(body.contains(term.as_str()))
                })
                .sum();
            (score > 0).then_some((score, document))
        })
        .collect();
    scored.sort_by(|(a_score, a), (b_score, b)| b_score.cmp(a_score).then(b.seq.cmp(&a.seq)));
    scored
        .into_iter()
        .take(MAX_HITS)
        .map(|(_, document)| Hit {
            seq: document.seq,
            name: document.name.to_owned(),
            heading: document.heading.to_owned(),
            line: matching_line(document.body, terms),
        })
        .collect()
}

fn matching_line(body: &str, terms: &[String]) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| {
            let line = line.to_lowercase();
            terms.iter().any(|term| line.contains(term.as_str()))
        })
        .map(clip)
}

fn clip(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_owned();
    }
    let cut = line.floor_char_boundary(MAX_LINE_BYTES - ELLIPSIS.len());
    format!("{}{ELLIPSIS}", &line[..cut])
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const HEADING: &str = "Heading";

    fn document<'a>(seq: u64, name: &'a str, body: &'a str) -> Document<'a> {
        Document {
            seq,
            name,
            heading: HEADING,
            body,
        }
    }

    fn names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.name.as_str()).collect()
    }

    #[test_case("Flaky-tests, make_check FLAKY", &["flaky", "make_check", "tests"] ; "words_lowercased_once")]
    #[test_case(" -- ", &[] ; "no_words")]
    fn query_splits_into_words(query: &str, expected: &[&str]) {
        assert_eq!(terms(query), expected);
    }

    #[test]
    fn name_match_outweighs_body_match() {
        let documents = [
            document(0, "flaky-tests.md", "Retry the suite."),
            document(1, "ci.md", "Flaky jobs on CI"),
            document(2, "release.md", "Nothing here"),
        ];

        let hits = search(&terms("flaky"), documents);

        assert_eq!(names(&hits), ["flaky-tests.md", "ci.md"]);
    }

    #[test]
    fn more_terms_outweigh_fewer() {
        let documents = [
            document(0, "a.md", "cache keys and prompts"),
            document(1, "b.md", "cache only"),
        ];

        let hits = search(&terms("cache prompts"), documents);

        assert_eq!(names(&hits), ["a.md", "b.md"]);
    }

    #[test]
    fn tie_goes_to_the_most_recent() {
        let documents = [
            document(0, "old.md", "cache"),
            document(3, "new.md", "cache"),
        ];

        let hits = search(&terms("cache"), documents);

        assert_eq!(names(&hits), ["new.md", "old.md"]);
    }

    #[test]
    fn hits_are_capped_at_the_most_recent() {
        let names_by_seq: Vec<String> = (0..15).map(|seq| format!("{seq}.md")).collect();
        let documents = (0..)
            .zip(&names_by_seq)
            .map(|(seq, name)| document(seq, name, "cache"));

        let hits = search(&terms("cache"), documents);

        assert_eq!(hits.len(), MAX_HITS);
        assert_eq!(hits.first().map(|hit| hit.seq), Some(14));
        assert_eq!(hits.last().map(|hit| hit.seq), Some(5));
    }

    #[test_case("intro\n  The Cache key  \nmore cache", Some("The Cache key") ; "first_matching_line_trimmed")]
    #[test_case("nothing", None ; "name_only_match")]
    fn hit_shows_the_first_matching_line(body: &str, expected: Option<&str>) {
        let hits = search(&terms("cache"), [document(0, "cache.md", body)]);

        assert_eq!(hits[0].line.as_deref(), expected);
    }

    #[test]
    fn long_matching_line_is_clipped() {
        let body = format!("cache {}", "é".repeat(MAX_LINE_BYTES));

        let hits = search(&terms("cache"), [document(0, "a.md", &body)]);

        let line = hits[0].line.as_deref().unwrap();
        assert!(line.len() <= MAX_LINE_BYTES);
        assert!(line.ends_with(ELLIPSIS));
    }
}
