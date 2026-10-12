//! Word search over the current notes: the way in to what the view only
//! names, or has folded into a line about something else.

use std::collections::BTreeSet;

/// Most hits one search returns.
pub const MAX_HITS: usize = 10;
/// Most keyword matches a search hands the decision engine to rank.
pub const POOL_HITS: usize = 32;
/// Most bytes of a candidate's body excerpt.
pub const EXCERPT_BYTES: usize = 800;
/// Most bytes of a hit's matching line.
pub const MAX_LINE_BYTES: usize = 200;

const NAME_WEIGHT: usize = 4;
const BODY_WEIGHT: usize = 1;
/// A note shares at least one word of its name or heading, or two of its
/// body, before it counts as related.
const MIN_RELATED_SCORE: usize = 2;
const MARKDOWN_SUFFIX: &str = ".md";
const ELLIPSIS: &str = "…";
/// Words too common to say which candidate a request wants, including the
/// "use" that skill descriptions open with by convention.
const STOPWORDS: &[&str] = &[
    "about", "an", "and", "any", "are", "as", "at", "be", "but", "by", "can", "could", "do",
    "does", "for", "from", "get", "has", "have", "help", "how", "if", "in", "into", "is", "it",
    "its", "like", "make", "me", "my", "need", "not", "of", "on", "or", "our", "please", "should",
    "so", "some", "that", "the", "their", "them", "then", "there", "these", "this", "those", "to",
    "up", "us", "use", "using", "want", "was", "we", "what", "when", "where", "which", "while",
    "who", "why", "will", "with", "would", "you", "your",
];

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

/// A note offered to the decision engine: the hit, and the start of its
/// body, at most [`EXCERPT_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub hit: Hit,
    pub excerpt: String,
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

/// The words of `text` that can say what it is about: lowercased runs of
/// letters and digits, two characters or longer, without the stopwords.
pub fn significant_words(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| word.chars().nth(1).is_some())
        .map(str::to_lowercase)
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
        .collect()
}

/// The words a note's name says about it, without the suffix every note
/// shares.
pub fn name_words(name: &str) -> BTreeSet<String> {
    significant_words(name.strip_suffix(MARKDOWN_SUFFIX).unwrap_or(name))
}

/// The notes that best match `terms`, at most [`MAX_HITS`]: a term in a
/// note's name counts more than one in its body, and the more recent note
/// wins a tie.
pub fn search<'a>(terms: &[String], documents: impl IntoIterator<Item = Document<'a>>) -> Vec<Hit> {
    best(matching(terms, documents), MAX_HITS)
        .map(|document| hit(&document, terms))
        .collect()
}

/// [`search`] widened to [`POOL_HITS`], each hit with its excerpt. Its first
/// [`MAX_HITS`] hits are exactly what [`search`] returns.
pub fn pool<'a>(
    terms: &[String],
    documents: impl IntoIterator<Item = Document<'a>>,
) -> Vec<Candidate> {
    best(matching(terms, documents), POOL_HITS)
        .map(|document| Candidate {
            hit: hit(&document, terms),
            excerpt: excerpt(document.body),
        })
        .collect()
}

/// The notes sharing the most of `words`, at most `limit`: whole words, a
/// word in a note's name or heading counting more than one in its body, and
/// the more recent note winning a tie.
pub fn related<'a>(
    words: &BTreeSet<String>,
    documents: impl IntoIterator<Item = Document<'a>>,
    limit: usize,
) -> Vec<Candidate> {
    let scored = documents
        .into_iter()
        .filter_map(|document| {
            let mut titled = name_words(document.name);
            titled.extend(significant_words(document.heading));
            let score = NAME_WEIGHT * titled.intersection(words).count()
                + BODY_WEIGHT * significant_words(document.body).intersection(words).count();
            (score >= MIN_RELATED_SCORE).then_some((score, document))
        })
        .collect();
    best(scored, limit)
        .map(|document| Candidate {
            hit: Hit {
                seq: document.seq,
                name: document.name.to_owned(),
                heading: document.heading.to_owned(),
                line: None,
            },
            excerpt: excerpt(document.body),
        })
        .collect()
}

fn matching<'a>(
    terms: &[String],
    documents: impl IntoIterator<Item = Document<'a>>,
) -> Vec<(usize, Document<'a>)> {
    documents
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
        .collect()
}

fn best<'a>(
    mut scored: Vec<(usize, Document<'a>)>,
    limit: usize,
) -> impl Iterator<Item = Document<'a>> {
    scored.sort_by(|(a_score, a), (b_score, b)| b_score.cmp(a_score).then(b.seq.cmp(&a.seq)));
    scored.into_iter().take(limit).map(|(_, document)| document)
}

fn hit(document: &Document<'_>, terms: &[String]) -> Hit {
    Hit {
        seq: document.seq,
        name: document.name.to_owned(),
        heading: document.heading.to_owned(),
        line: matching_line(document.body, terms),
    }
}

fn excerpt(body: &str) -> String {
    body[..body.floor_char_boundary(EXCERPT_BYTES)].to_owned()
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
    fn the_pool_widens_the_search_and_starts_with_its_hits() {
        let names_by_seq: Vec<String> = (0..POOL_HITS + 5).map(|seq| format!("{seq}.md")).collect();
        let documents = || {
            (0..)
                .zip(&names_by_seq)
                .map(|(seq, name)| document(seq, name, "cache"))
        };

        let pool = pool(&terms("cache"), documents());

        assert_eq!(pool.len(), POOL_HITS);
        let hits: Vec<Hit> = pool.into_iter().map(|candidate| candidate.hit).collect();
        assert_eq!(hits[..MAX_HITS], search(&terms("cache"), documents()));
    }

    #[test]
    fn an_excerpt_is_cut_on_a_char_boundary() {
        let body = format!("a{}", "é".repeat(EXCERPT_BYTES));

        let pool = pool(&terms("a"), [document(0, "a.md", &body)]);

        assert!(pool[0].excerpt.len() <= EXCERPT_BYTES);
        assert!(body.starts_with(&pool[0].excerpt));
    }

    #[test_case("Fix the flaky-tests, then RELEASE", &["fix", "flaky", "release", "tests"] ; "stopwords_dropped")]
    #[test_case("a b c 1", &[] ; "single_characters_dropped")]
    fn significant_words_skip_what_says_nothing(text: &str, expected: &[&str]) {
        assert_eq!(
            significant_words(text),
            BTreeSet::from_iter(expected.iter().map(|word| word.to_string()))
        );
    }

    #[test_case("release tagging", &["release.md"] ; "a_title_word_is_enough")]
    #[test_case("tags signed", &["notes.md"] ; "two_body_words_are_enough")]
    #[test_case("signed", &[] ; "one_body_word_is_not")]
    #[test_case("md", &[] ; "the_markdown_suffix_says_nothing")]
    #[test_case("rel", &[] ; "only_whole_words_count")]
    fn related_notes_share_whole_words(text: &str, expected: &[&str]) {
        let documents = [
            document(0, "release.md", "Cut from main."),
            document(1, "notes.md", "Tags are signed."),
        ];

        let related = related(&significant_words(text), documents, MAX_HITS);

        let names: Vec<&str> = related
            .iter()
            .map(|candidate| candidate.hit.name.as_str())
            .collect();
        assert_eq!(names, expected);
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
