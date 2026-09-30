//! Full-text search over every section. Terms are ASCII case-insensitive substrings that must all occur in one
//! section. A term that starts no word anywhere looks like a typo, so it is also matched against the nearest
//! indexed words. The corpus is small enough that one scan per term beats keeping an inverted index.

use std::collections::HashMap;
use std::ops::Range;

use memchr::memmem;

use crate::Page;
use crate::markdown::{self, Fences};

const ONE_TYPO_MIN_LEN: usize = 5;
const TWO_TYPOS_MIN_LEN: usize = 9;
const ONE_TYPO: usize = 1;
const TWO_TYPOS: usize = 2;
const MAX_ALTERNATIVES: usize = 3;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const SNIPPET_CHARS: usize = 160;
const SNIPPET_LEAD_CHARS: usize = 40;
const ELLIPSIS: &str = "…";
const WHOLE_WORD: u8 = 2;
const WORD_START: u8 = 1;
const INSIDE_WORD: u8 = 0;
const TABLE_RULE_CHARS: [char; 4] = ['|', '-', ':', ' '];

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Search {
    pub hits: Vec<Hit>,
    /// How many sections matched. `hits` holds the best of them.
    pub matched: usize,
    pub corrections: Vec<Correction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub page: usize,
    pub heading: usize,
    pub snippet: String,
    /// Byte ranges of `snippet` that matched a term.
    pub highlights: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    pub typed: String,
    pub used: String,
}

pub(crate) struct SearchIndex {
    sections: Vec<Section>,
    text: String,
    folded: String,
    vocabulary: Vec<(String, u32)>,
    average_len: f64,
}

/// One heading and its own text, up to the next heading of any level, so a hit belongs to the most specific
/// section only.
struct Section {
    page: usize,
    heading: usize,
    body: Range<usize>,
    title: String,
}

#[derive(Default, Clone, Copy)]
struct SectionMatch {
    count: u32,
    quality: u8,
    in_title: bool,
}

struct TermMatches {
    needles: Vec<String>,
    sections: HashMap<usize, SectionMatch>,
}

struct Ranked {
    section: usize,
    title_terms: u32,
    quality: u32,
    score: f64,
}

impl SectionMatch {
    fn record(&mut self, quality: u8, in_title: bool) {
        self.count += 1;
        self.quality = self.quality.max(quality);
        self.in_title |= in_title;
    }
}

impl TermMatches {
    fn merge(&mut self, other: TermMatches) {
        self.needles.extend(other.needles);
        for (section, found) in other.sections {
            let entry = self.sections.entry(section).or_default();
            entry.count += found.count;
            entry.quality = entry.quality.max(found.quality);
            entry.in_title |= found.in_title;
        }
    }
}

impl SearchIndex {
    pub(crate) fn build(pages: &[Page]) -> Self {
        let mut text = String::new();
        let mut sections = Vec::new();
        for (page_index, page) in pages.iter().enumerate() {
            let mut fences = Fences::default();
            let plain: Vec<String> = page
                .body
                .lines()
                .map(|line| plain_line(line, &mut fences))
                .collect();
            for (heading_index, heading) in page.headings.iter().enumerate() {
                let end = page
                    .headings
                    .get(heading_index + 1)
                    .map_or(plain.len(), |next| next.line);
                let start = text.len();
                for line in &plain[heading.line + 1..end] {
                    text.push_str(line);
                    text.push('\n');
                }
                sections.push(Section {
                    page: page_index,
                    heading: heading_index,
                    body: start..text.len(),
                    title: heading.title.to_ascii_lowercase(),
                });
            }
        }
        let folded = text.to_ascii_lowercase();
        let vocabulary = vocabulary(&folded, &sections);
        let total_len: usize = sections
            .iter()
            .map(|section| section.body.len() + section.title.len())
            .sum();
        let average_len = (total_len as f64 / sections.len().max(1) as f64).max(1.0);
        Self {
            sections,
            text,
            folded,
            vocabulary,
            average_len,
        }
    }

    pub(crate) fn search(&self, query: &str, limit: usize) -> Search {
        let terms = terms(query);
        let mut corrections = Vec::new();
        let matches: Vec<TermMatches> = terms
            .iter()
            .map(|term| {
                let mut found = self.find(term, INSIDE_WORD);
                let starts_no_word = found
                    .sections
                    .values()
                    .all(|section| section.quality == INSIDE_WORD);
                if may_be_typo(term) && starts_no_word {
                    let alternatives = self.alternatives(term);
                    if let Some(best) = alternatives.first() {
                        corrections.push(Correction {
                            typed: term.clone(),
                            used: best.clone(),
                        });
                    }
                    for alternative in alternatives {
                        found.merge(self.find(&alternative, WHOLE_WORD));
                    }
                }
                found
            })
            .collect();
        let mut ranked = self.rank(&matches);
        let matched = ranked.len();
        ranked.truncate(limit);
        Search {
            hits: ranked
                .iter()
                .map(|rank| self.hit(rank.section, &matches))
                .collect(),
            matched,
            corrections,
        }
    }

    fn find(&self, needle: &str, min_quality: u8) -> TermMatches {
        let finder = memmem::Finder::new(needle);
        let mut sections: HashMap<usize, SectionMatch> = HashMap::new();
        for at in finder.find_iter(self.folded.as_bytes()) {
            let quality = quality(&self.folded, at, needle.len());
            if quality >= min_quality {
                sections
                    .entry(self.section_at(at))
                    .or_default()
                    .record(quality, false);
            }
        }
        for (index, section) in self.sections.iter().enumerate() {
            for at in finder.find_iter(section.title.as_bytes()) {
                let quality = quality(&section.title, at, needle.len());
                if quality >= min_quality {
                    sections.entry(index).or_default().record(quality, true);
                }
            }
        }
        TermMatches {
            needles: vec![needle.to_owned()],
            sections,
        }
    }

    fn section_at(&self, at: usize) -> usize {
        self.sections
            .partition_point(|section| section.body.end <= at)
    }

    /// Indexed words within Meilisearch's typo budget of `term`, nearest and most widespread first. The first
    /// letter must match, which keeps short words from drifting into unrelated ones.
    fn alternatives(&self, term: &str) -> Vec<String> {
        let budget = if term.len() >= TWO_TYPOS_MIN_LEN {
            TWO_TYPOS
        } else {
            ONE_TYPO
        };
        let mut near: Vec<(usize, u32, &str)> = self
            .vocabulary
            .iter()
            .filter(|(word, _)| {
                word.as_bytes().first() == term.as_bytes().first()
                    && word.len().abs_diff(term.len()) <= budget
                    && word.as_str() != term
            })
            .filter_map(|(word, sections)| {
                let distance = strsim::osa_distance(term, word);
                (distance <= budget).then_some((distance, *sections, word.as_str()))
            })
            .collect();
        near.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
        near.into_iter()
            .take(MAX_ALTERNATIVES)
            .map(|(_, _, word)| word.to_owned())
            .collect()
    }

    /// Sections holding every term: most terms in the heading first, then the best word boundaries, then BM25,
    /// then document order.
    fn rank(&self, matches: &[TermMatches]) -> Vec<Ranked> {
        let Some((first, rest)) = matches.split_first() else {
            return Vec::new();
        };
        let total = self.sections.len() as f64;
        let idf: Vec<f64> = matches
            .iter()
            .map(|term| {
                let df = term.sections.len() as f64;
                (1.0 + (total - df + 0.5) / (df + 0.5)).ln()
            })
            .collect();
        let mut ranked: Vec<Ranked> = first
            .sections
            .keys()
            .copied()
            .filter(|section| rest.iter().all(|term| term.sections.contains_key(section)))
            .map(|section| self.score(section, matches, &idf))
            .collect();
        ranked.sort_by(|a, b| {
            b.title_terms
                .cmp(&a.title_terms)
                .then(b.quality.cmp(&a.quality))
                .then(b.score.total_cmp(&a.score))
                .then(a.section.cmp(&b.section))
        });
        ranked
    }

    fn score(&self, section: usize, matches: &[TermMatches], idf: &[f64]) -> Ranked {
        let entry = &self.sections[section];
        let len = (entry.body.len() + entry.title.len()) as f64;
        let saturation = BM25_K1 * (1.0 - BM25_B + BM25_B * len / self.average_len);
        let mut ranked = Ranked {
            section,
            title_terms: 0,
            quality: 0,
            score: 0.0,
        };
        for (term, idf) in matches.iter().zip(idf) {
            let found = term.sections[&section];
            ranked.title_terms += u32::from(found.in_title);
            ranked.quality += u32::from(found.quality);
            let tf = f64::from(found.count);
            ranked.score += idf * tf * (BM25_K1 + 1.0) / (tf + saturation);
        }
        ranked
    }

    fn hit(&self, section: usize, matches: &[TermMatches]) -> Hit {
        let entry = &self.sections[section];
        let (snippet, highlights) = snippet(
            &self.text[entry.body.clone()],
            &self.folded[entry.body.clone()],
            matches,
        );
        Hit {
            page: entry.page,
            heading: entry.heading,
            snippet,
            highlights,
        }
    }
}

fn terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for term in query.split_whitespace().map(str::to_ascii_lowercase) {
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn may_be_typo(term: &str) -> bool {
    term.len() >= ONE_TYPO_MIN_LEN && term.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn quality(text: &str, at: usize, len: usize) -> u8 {
    let bytes = text.as_bytes();
    let starts = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
    let ends = at + len == bytes.len() || !bytes[at + len].is_ascii_alphanumeric();
    match (starts, ends) {
        (true, true) => WHOLE_WORD,
        (true, false) => WORD_START,
        _ => INSIDE_WORD,
    }
}

fn plain_line(line: &str, fences: &mut Fences) -> String {
    let was_open = fences.is_open();
    if fences.step(line) {
        return if was_open && fences.is_open() {
            line.to_owned()
        } else {
            String::new()
        };
    }
    if let Some((_, heading)) = markdown::atx_heading(line) {
        return markdown::plain_inline(markdown::split_explicit_id(heading).0);
    }
    let trimmed = line.trim_start();
    if !trimmed.starts_with('|') {
        return markdown::plain_inline(line);
    }
    if trimmed.chars().all(|ch| TABLE_RULE_CHARS.contains(&ch)) {
        return String::new();
    }
    markdown::plain_inline(line).replace('|', " ")
}

fn vocabulary(folded: &str, sections: &[Section]) -> Vec<(String, u32)> {
    let mut counts: HashMap<&str, (u32, usize)> = HashMap::new();
    for (index, section) in sections.iter().enumerate() {
        let words = words(&folded[section.body.clone()]).chain(words(&section.title));
        for word in words.filter(|word| word.len() >= ONE_TYPO_MIN_LEN) {
            let (count, last_section) = counts.entry(word).or_insert((0, usize::MAX));
            if *last_section != index {
                *count += 1;
                *last_section = index;
            }
        }
    }
    counts
        .into_iter()
        .map(|(word, (count, _))| (word.to_owned(), count))
        .collect()
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
}

/// The line holding the most distinct terms, cut to a window around its first hit.
fn snippet(text: &str, folded: &str, matches: &[TermMatches]) -> (String, Vec<Range<usize>>) {
    let mut best: Option<(usize, &str, &str)> = None;
    for (line, folded_line) in text.lines().zip(folded.lines()) {
        if line.trim().is_empty() {
            continue;
        }
        let distinct = matches
            .iter()
            .filter(|term| {
                term.needles
                    .iter()
                    .any(|needle| folded_line.contains(needle.as_str()))
            })
            .count();
        if best.is_none_or(|(most, _, _)| distinct > most) {
            best = Some((distinct, line, folded_line));
        }
    }
    best.map_or_else(
        || (String::new(), Vec::new()),
        |(_, line, folded_line)| cut(line, folded_line, matches),
    )
}

fn cut(line: &str, folded: &str, matches: &[TermMatches]) -> (String, Vec<Range<usize>>) {
    let indent = line.len() - line.trim_start().len();
    let end_of_text = line.trim_end().len();
    let (line, folded) = (&line[indent..end_of_text], &folded[indent..end_of_text]);
    let mut ranges: Vec<Range<usize>> = matches
        .iter()
        .flat_map(|term| &term.needles)
        .flat_map(|needle| {
            memmem::find_iter(folded.as_bytes(), needle.as_bytes())
                .map(move |at| at..at + needle.len())
        })
        .collect();
    ranges.sort_by_key(|range| (range.start, range.end));
    let first = ranges.first().map_or(0, |range| range.start);
    let start = line[..first]
        .char_indices()
        .rev()
        .nth(SNIPPET_LEAD_CHARS - 1)
        .map_or(0, |(at, _)| at);
    let end = line[start..]
        .char_indices()
        .nth(SNIPPET_CHARS)
        .map_or(line.len(), |(at, _)| start + at);
    let prefix = if start > 0 { ELLIPSIS } else { "" };
    let mut snippet = format!("{prefix}{}", &line[start..end]);
    if end < line.len() {
        snippet.push_str(ELLIPSIS);
    }
    let shift = prefix.len();
    let highlights = merge(
        ranges
            .into_iter()
            .filter(|range| range.start >= start && range.end <= end)
            .map(|range| range.start - start + shift..range.end - start + shift),
    );
    (snippet, highlights)
}

fn merge(ranges: impl Iterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{Correction, SNIPPET_CHARS, Search};
    use crate::Library;
    use crate::fixture::{leak, library};

    const LIMIT: usize = 10;

    fn one_page(body: &str) -> Library {
        library(&[("page", leak(format!("# Page\n\n{body}")))])
    }

    fn titles(library: &Library, search: &Search) -> Vec<String> {
        search
            .hits
            .iter()
            .map(|hit| {
                library.pages()[hit.page].headings[hit.heading]
                    .title
                    .clone()
            })
            .collect()
    }

    fn highlighted(search: &Search) -> Vec<&str> {
        let hit = &search.hits[0];
        hit.highlights
            .iter()
            .map(|range| &hit.snippet[range.clone()])
            .collect()
    }

    #[test]
    fn terms_match_case_insensitively() {
        let library = one_page("## One\n\nPermissions matter.\n");
        let search = library.search("PERMISSIONS", LIMIT);
        assert_eq!(titles(&library, &search), ["One"]);
    }

    #[test]
    fn every_term_must_match_in_one_section() {
        let library = one_page("## One\n\nshell here\n\n## Two\n\ntimeout here\n");
        assert_eq!(library.search("shell timeout", LIMIT).matched, 0);
        assert_eq!(library.search("shell", LIMIT).matched, 1);
    }

    #[test]
    fn heading_match_outranks_body_match() {
        let library = one_page("## Other\n\nshell shell shell\n\n## Shell\n\nabout it\n");
        let search = library.search("shell", LIMIT);
        assert_eq!(titles(&library, &search), ["Shell", "Other"]);
    }

    #[test]
    fn whole_word_outranks_word_start_outranks_inside_word() {
        let library = one_page("## A\n\na subshell\n\n## B\n\nshells\n\n## C\n\nthe shell\n");
        let search = library.search("shell", LIMIT);
        assert_eq!(titles(&library, &search), ["C", "B", "A"]);
    }

    #[test]
    fn unknown_term_falls_back_to_nearest_word() {
        let library = one_page("## A\n\npermissions apply\n");
        let search = library.search("permisions", LIMIT);
        assert_eq!(titles(&library, &search), ["A"]);
        assert_eq!(
            search.corrections,
            [Correction {
                typed: "permisions".into(),
                used: "permissions".into()
            }]
        );
    }

    #[test]
    fn term_only_inside_words_also_tries_typos() {
        let library = one_page("## A\n\navailable everywhere\n\n## B\n\na label here\n");
        let search = library.search("lable", LIMIT);
        assert_eq!(titles(&library, &search), ["B", "A"]);
        assert_eq!(search.corrections[0].used, "label");
    }

    #[test_case("shle", 0 ; "under five bytes never falls back")]
    #[test_case("shlel", 1 ; "one edit from five bytes")]
    #[test_case("shlle", 0 ; "two edits need nine bytes")]
    #[test_case("hsell", 0 ; "first letter must match")]
    #[test_case("confguraton", 1 ; "two edits from nine bytes")]
    #[test_case("cnfguraton", 0 ; "three edits are too many")]
    fn typo_budget(term: &str, matched: usize) {
        let library = one_page("## A\n\nshell configuration\n");
        assert_eq!(library.search(term, LIMIT).matched, matched);
    }

    #[test]
    fn snippet_highlights_every_term() {
        let library = one_page("## A\n\nThe shell stops after a timeout.\n");
        let search = library.search("timeout SHELL", LIMIT);
        assert_eq!(search.hits[0].snippet, "The shell stops after a timeout.");
        assert_eq!(highlighted(&search), ["shell", "timeout"]);
    }

    #[test]
    fn snippet_cuts_on_char_boundaries() {
        let line = format!("{} shell {}", "é".repeat(100), "ü".repeat(300));
        let library = one_page(&format!("## A\n\n{line}\n"));
        let search = library.search("shell", LIMIT);
        let snippet = &search.hits[0].snippet;
        assert!(
            snippet.starts_with('…') && snippet.ends_with('…'),
            "{snippet}"
        );
        assert!(snippet.chars().count() <= SNIPPET_CHARS + 2);
        assert_eq!(highlighted(&search), ["shell"]);
    }

    #[test]
    fn blank_query_finds_nothing() {
        let library = one_page("## A\n\ntext\n");
        assert_eq!(library.search("   ", LIMIT), Search::default());
    }
}
