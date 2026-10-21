//! Text for the model: the index the skill returns, the report for `caudra-docs?<terms>`, and the modal's
//! contents page, which shares the grouping.

use std::fmt::Write;

use crate::{
    DocsError, Heading, Library, NAME, PAGE_TITLE_LEVEL, Page, SECTION_LEVEL, SITE_DOCS_URL, Search,
};

const BYTES_PER_TOKEN: usize = 4;
const TOKENS_PER_K: usize = 1_000;
const NESTED_LISTING_TOKENS: usize = 3_000;
const SUBSECTION_LEVEL: u8 = 3;
const MODEL_SEARCH_LIMIT: usize = 8;
const CORPUS: &str = "the docs";
const RESULT_UNIT: &str = "section";
const ADDRESSING: &str = "This build's user documentation. Load a page with the skill name \
`caudra-docs/<page>`, or one section with `caudra-docs/<page>#<section>`, which costs far less. A link such as \
`/docs/<page>/#<section>` in the text means `caudra-docs/<page>#<section>`, and a heading ending in `{#id}` is the \
section `id`. `caudra-docs?<terms>` lists the sections that contain every term, each with its address and a \
snippet. Token sizes are estimates.";

pub(crate) fn render(library: &Library) -> String {
    let mut index = format!("# {NAME} {}\n\n{ADDRESSING}\n", env!("CARGO_PKG_VERSION"));
    for_each_page(library, &mut index, |index, page| {
        let _ = writeln!(
            index,
            "- {}: {}, {} tokens. {}",
            page.slug,
            page.title,
            approx_tokens(page.body.len()),
            page.description
        );
        let sections: Vec<String> = page
            .headings
            .iter()
            .enumerate()
            .filter(|(_, heading)| heading.level == SECTION_LEVEL)
            .map(|(position, heading)| section_entry(page, position, heading))
            .collect();
        if !sections.is_empty() {
            let _ = writeln!(index, "  {}", sections.join(", "));
        }
    });
    index
}

pub(crate) fn contents(library: &Library) -> String {
    let mut contents = format!(
        "# Caudra docs\n\nThe user manual that shipped with this build, version {}.\n",
        env!("CARGO_PKG_VERSION")
    );
    for_each_page(library, &mut contents, |contents, page| {
        let _ = writeln!(
            contents,
            "- [{}]({SITE_DOCS_URL}{}/): {}",
            page.title, page.slug, page.description
        );
    });
    contents
}

pub(crate) fn search_report(library: &Library, query: &str) -> Result<String, DocsError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(DocsError::EmptyQuery);
    }
    let search = library.search(query, MODEL_SEARCH_LIMIT);
    let no_match = format!(
        "No section contains every term of \"{query}\". Use fewer terms, or load {NAME} for the index."
    );
    let mut report = SearchReport::new(&search, query, CORPUS, RESULT_UNIT, &no_match);
    for hit in &search.hits {
        let page = &library.pages()[hit.page];
        let heading = &page.headings[hit.heading];
        let place = if heading.level == PAGE_TITLE_LEVEL {
            page.title.clone()
        } else {
            format!("{} › {}", page.title, heading.title)
        };
        report.entry(
            &format!("{NAME}/{}#{}", page.slug, heading.anchor),
            Some(&place),
            page.section(hit.heading).len(),
            &hit.snippet,
        );
    }
    Ok(report.finish())
}

/// A search's answer for the model: a line per correction, how many results it lists, then each result with its
/// address, size, and snippet.
pub struct SearchReport {
    text: String,
}

impl SearchReport {
    /// The report's head for a search over `corpus` whose results are each a `unit`. `no_match` replaces the count
    /// when `search` has no hits.
    pub fn new(search: &Search, query: &str, corpus: &str, unit: &str, no_match: &str) -> Self {
        let mut text = String::new();
        for correction in &search.corrections {
            let _ = writeln!(
                text,
                "No word in {corpus} starts with \"{}\", so it also matched \"{}\".",
                correction.typed, correction.used
            );
        }
        let shown = search.hits.len();
        let _ = match shown {
            0 => writeln!(text, "{no_match}"),
            1 => writeln!(
                text,
                "1 {unit} matches \"{query}\". Load it by its address."
            ),
            _ if shown < search.matched => writeln!(
                text,
                "{shown} of {} {unit}s match \"{query}\". Load one by its address.",
                search.matched
            ),
            _ => writeln!(
                text,
                "{shown} {unit}s match \"{query}\". Load one by its address."
            ),
        };
        Self { text }
    }

    /// One result: its address, where it sits when the address alone does not say, its size, and its snippet.
    pub fn entry(&mut self, address: &str, place: Option<&str>, bytes: usize, snippet: &str) {
        let tokens = approx_tokens(bytes);
        let _ = match place {
            Some(place) => writeln!(self.text, "- {address} ({place}, {tokens} tokens)"),
            None => writeln!(self.text, "- {address} ({tokens} tokens)"),
        };
        if !snippet.is_empty() {
            let _ = writeln!(self.text, "  {snippet}");
        }
    }

    pub fn finish(self) -> String {
        self.text.trim_end().to_owned()
    }
}

fn for_each_page(
    library: &Library,
    out: &mut String,
    mut write_page: impl FnMut(&mut String, &Page),
) {
    let mut group = None;
    for page in library.pages() {
        if group != Some(page.group.as_str()) {
            group = Some(page.group.as_str());
            let _ = write!(out, "\n## {}\n\n", page.group);
        }
        write_page(out, page);
    }
}

fn section_entry(page: &Page, position: usize, heading: &Heading) -> String {
    if page.section(position).len().div_ceil(BYTES_PER_TOKEN) <= NESTED_LISTING_TOKENS {
        return heading.anchor.clone();
    }
    let subsections: Vec<&str> = page.headings[position + 1..]
        .iter()
        .take_while(|next| next.level > SECTION_LEVEL)
        .filter(|next| next.level == SUBSECTION_LEVEL)
        .map(|next| next.anchor.as_str())
        .collect();
    if subsections.is_empty() {
        heading.anchor.clone()
    } else {
        format!("{} ({})", heading.anchor, subsections.join(", "))
    }
}

fn approx_tokens(bytes: usize) -> String {
    let tokens = bytes.div_ceil(BYTES_PER_TOKEN);
    if tokens < TOKENS_PER_K {
        format!("~{tokens}")
    } else {
        format!("~{:.1}k", tokens as f64 / TOKENS_PER_K as f64)
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use crate::site;
    use crate::{DocsError, NAME};

    const ADDRESS_PREFIX: &str = "- caudra-docs/";

    fn addresses(report: &str) -> Vec<&str> {
        report
            .lines()
            .filter_map(|line| line.strip_prefix(ADDRESS_PREFIX))
            .filter_map(|rest| rest.split_once(" (").map(|(address, _)| address))
            .collect()
    }

    #[test]
    fn index_lists_every_page_and_section_and_they_load() {
        let library = site::library();
        let index = library.index();
        for page in library.pages() {
            assert!(
                index.contains(&format!("- {}: ", page.slug)),
                "{} is missing",
                page.slug
            );
            for heading in page.headings.iter().filter(|heading| heading.level == 2) {
                assert!(
                    index.contains(&heading.anchor),
                    "{}#{} is missing",
                    page.slug,
                    heading.anchor
                );
                let address = format!("/{}#{}", page.slug, heading.anchor);
                assert!(library.load(&address).is_ok(), "{address} does not load");
            }
        }
    }

    #[test_case("shell timeout" ; "two terms")]
    #[test_case("permisions" ; "typo")]
    #[test_case("plugins.skill workflow_dev" ; "config key")]
    fn model_search_lists_addresses_that_load(query: &str) {
        let library = site::library();
        let report = library.load(&format!("?{query}")).expect("search report");
        let found = addresses(&report);
        assert!(!found.is_empty(), "no results for {query}:\n{report}");
        for address in found {
            assert!(
                library.load(&format!("/{address}")).is_ok(),
                "{NAME}/{address} does not load"
            );
        }
    }

    #[test]
    fn model_search_reports_the_correction() {
        let report = site::library().load("?permisions").expect("search report");
        assert!(
            report.contains("\"permisions\", so it also matched \"permissions\""),
            "{report}"
        );
    }

    #[test]
    fn empty_query_is_an_error() {
        assert_eq!(site::library().load("?  "), Err(DocsError::EmptyQuery));
    }
}
