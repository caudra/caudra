//! Caudra's user documentation as data: the pages under `site/docs/content`, their sections and addresses, the
//! index the model reads, the text the TUI renders, and a search over all of it.
//!
//! Nothing is embedded here. The binary embeds the Markdown and hands it to [`Library::parse`], so a docs edit
//! rebuilds the binary alone rather than every crate that reads the docs.

mod display;
mod index;
mod markdown;
mod search;
mod slug;

use std::borrow::Cow;
use std::ops::Range;
use std::sync::OnceLock;

use thiserror::Error;

pub use search::{Correction, Hit, Search};

pub const NAME: &str = "caudra-docs";
pub const SKILL_DESCRIPTION: &str = "Caudra's own user documentation for this build: caudra.toml \
configuration, permissions, tools, commands, keybindings, providers, sessions, MCP, skills, plugins, workflows. \
Load it before answering questions about using or configuring Caudra.";
pub const SITE_DOCS_URL: &str = "https://caudra.ai/docs/";

const DOCS_PATH: &str = "/docs/";
const LANDING_PAGE: &str = "_index.md";
const FRONT_MATTER_FENCE: &str = "+++";
const PAGE_TITLE_LEVEL: u8 = 1;
const SECTION_LEVEL: u8 = 2;
const EYEBROW_CLASS: &str = "eyebrow";
const CARD_TITLE_CLASS: &str = "card-title";
const CARD_DESCRIPTION_CLASS: &str = "card-desc";
const CARD_MARKER: &str = "class=\"card\"";
const HREF_ATTRIBUTE: &str = "href=\"";

/// How the TUI reaches the library the binary parses on first use.
pub type DocsLibrary = fn() -> &'static Library;

pub struct Library {
    pages: Vec<Page>,
    search: OnceLock<search::SearchIndex>,
}

#[derive(Debug)]
pub struct Page {
    pub slug: &'static str,
    pub title: String,
    pub description: String,
    pub group: String,
    /// The Markdown without its front matter and HTML comments, as the model reads it.
    pub body: Cow<'static, str>,
    /// The Markdown the TUI renders, line for line the same as `body`.
    pub display: String,
    pub headings: Vec<Heading>,
    line_starts: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub title: String,
    pub anchor: String,
    /// Zero-based line in `body` and `display`.
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub page: usize,
    pub heading: Option<usize>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DocsError {
    #[error("There is no docs page `{page}`. Pages: {available}.")]
    UnknownPage { page: String, available: String },
    #[error("Page `{page}` has no section `{anchor}`. Its sections: {available}.")]
    UnknownSection {
        page: String,
        anchor: String,
        available: String,
    },
    #[error("Add at least one search term after `?`, as in `caudra-docs?shell timeout`.")]
    EmptyQuery,
}

struct Card {
    slug: &'static str,
    title: String,
    description: String,
    group: String,
}

impl Library {
    /// Builds the library from `(path, contents)` pairs relative to `site/docs/content`. The landing page supplies
    /// the order, groups, titles and one-line descriptions; each `<slug>/_index.md` is a page.
    pub fn parse(files: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
        let mut landing = "";
        let mut sources = Vec::new();
        for (path, text) in files {
            if path == LANDING_PAGE {
                landing = text;
            } else if let Some(slug) = page_slug(path) {
                sources.push((slug, text));
            }
        }
        let cards = cards(landing);
        let card_position = |slug: &str| {
            cards
                .iter()
                .position(|card| card.slug == slug)
                .unwrap_or(usize::MAX)
        };
        sources.sort_by_key(|&(slug, _)| (card_position(slug), slug));
        let pages = sources
            .into_iter()
            .map(|(slug, text)| {
                Page::parse(slug, text, cards.iter().find(|card| card.slug == slug))
            })
            .collect();
        Self {
            pages,
            search: OnceLock::new(),
        }
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// What loading the `caudra-docs` skill returns: the addressing rules and every page with its sections.
    pub fn index(&self) -> String {
        index::render(self)
    }

    /// The text behind a skill address, given without the skill name: `/<page>`, `/<page>#<section>`, or
    /// `?<terms>` for a search.
    pub fn load(&self, address: &str) -> Result<String, DocsError> {
        if let Some(query) = address.strip_prefix('?') {
            return index::search_report(self, query);
        }
        let target = self.resolve(address.strip_prefix('/').unwrap_or(address))?;
        let page = &self.pages[target.page];
        Ok(target
            .heading
            .map_or_else(|| page.body.trim(), |heading| page.section(heading))
            .to_owned())
    }

    /// Where a link or typed address points: `<page>`, `<page>#<section>`, `/docs/<page>/#<section>`, or a
    /// caudra.ai docs URL. An unknown section still finds its page.
    pub fn locate(&self, target: &str) -> Option<Target> {
        let target = target.trim();
        let path = target
            .strip_prefix(SITE_DOCS_URL)
            .or_else(|| target.strip_prefix(DOCS_PATH))
            .unwrap_or(target);
        match self.resolve(path) {
            Ok(found) => Some(found),
            Err(DocsError::UnknownSection { .. }) => {
                self.page_index(split_address(path).0).map(|page| Target {
                    page,
                    heading: None,
                })
            }
            Err(_) => None,
        }
    }

    /// Sections that contain every term of `query`, best first. The index behind it is built on the first call.
    pub fn search(&self, query: &str, limit: usize) -> Search {
        self.search
            .get_or_init(|| search::SearchIndex::build(&self.pages))
            .search(query, limit)
    }

    /// The modal's home page: every page as a link with its description, grouped as on the landing page.
    pub fn contents_markdown(&self) -> String {
        index::contents(self)
    }

    fn resolve(&self, path: &str) -> Result<Target, DocsError> {
        let (slug, anchor) = split_address(path);
        let page = self
            .page_index(slug)
            .ok_or_else(|| DocsError::UnknownPage {
                page: slug.to_owned(),
                available: self
                    .pages
                    .iter()
                    .map(|page| page.slug)
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
        let Some(anchor) = anchor else {
            return Ok(Target {
                page,
                heading: None,
            });
        };
        let headings = &self.pages[page].headings;
        headings
            .iter()
            .position(|heading| heading.anchor == anchor)
            .or_else(|| {
                headings
                    .iter()
                    .position(|heading| heading.anchor.eq_ignore_ascii_case(anchor))
            })
            .map(|heading| Target {
                page,
                heading: Some(heading),
            })
            .ok_or_else(|| DocsError::UnknownSection {
                page: slug.to_owned(),
                anchor: anchor.to_owned(),
                available: self.pages[page].section_anchors().join(", "),
            })
    }

    fn page_index(&self, slug: &str) -> Option<usize> {
        self.pages
            .iter()
            .position(|page| page.slug.eq_ignore_ascii_case(slug))
    }
}

impl Page {
    fn parse(slug: &'static str, text: &'static str, card: Option<&Card>) -> Self {
        let body = markdown::without_comments(strip_front_matter(text));
        let headings = markdown::headings(&body);
        let title = card
            .map(|card| card.title.clone())
            .or_else(|| headings.first().map(|heading| heading.title.clone()))
            .unwrap_or_else(|| slug.to_owned());
        Self {
            slug,
            title,
            description: card
                .map(|card| card.description.clone())
                .unwrap_or_default(),
            group: card.map(|card| card.group.clone()).unwrap_or_default(),
            display: display::render(&body, slug),
            line_starts: line_starts(&body),
            body,
            headings,
        }
    }

    /// Heading `index` and its text up to the next heading of the same or a higher level. The page title is the
    /// exception: it ends at the first subsection, because the whole page has an address of its own.
    pub fn section(&self, index: usize) -> &str {
        let lines = self.section_lines(index);
        let start = self.line_starts[lines.start];
        let end = self
            .line_starts
            .get(lines.end)
            .copied()
            .unwrap_or(self.body.len());
        self.body[start..end].trim_end()
    }

    fn section_lines(&self, index: usize) -> Range<usize> {
        let heading = &self.headings[index];
        let end = self.headings[index + 1..]
            .iter()
            .find(|next| heading.level == PAGE_TITLE_LEVEL || next.level <= heading.level)
            .map_or(self.line_starts.len(), |next| next.line);
        heading.line..end
    }

    fn section_anchors(&self) -> Vec<&str> {
        self.headings
            .iter()
            .filter(|heading| heading.level == SECTION_LEVEL)
            .map(|heading| heading.anchor.as_str())
            .collect()
    }
}

/// Byte offset of every line start, for text split the way `str::lines` splits it.
pub fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            text.match_indices('\n')
                .map(|(index, _)| index + 1)
                .filter(|&start| start < text.len()),
        )
        .collect()
}

fn page_slug(path: &str) -> Option<&str> {
    let slug = path.strip_suffix(LANDING_PAGE)?.strip_suffix(['/', '\\'])?;
    (!slug.is_empty() && !slug.contains(['/', '\\'])).then_some(slug)
}

fn split_address(path: &str) -> (&str, Option<&str>) {
    let (page, anchor) = path
        .split_once('#')
        .map_or((path, None), |(page, anchor)| (page, Some(anchor)));
    (
        page.trim_end_matches('/'),
        anchor.filter(|anchor| !anchor.is_empty()),
    )
}

fn strip_front_matter(text: &str) -> &str {
    let Some(rest) = text
        .strip_prefix(FRONT_MATTER_FENCE)
        .and_then(|rest| rest.strip_prefix('\n'))
    else {
        return text;
    };
    let closing = format!("\n{FRONT_MATTER_FENCE}");
    rest.find(&closing).map_or(text, |end| {
        rest[end + closing.len()..].trim_start_matches(['\r', '\n'])
    })
}

fn cards(landing: &'static str) -> Vec<Card> {
    let mut group = String::new();
    let mut cards = Vec::new();
    for line in landing.lines() {
        if let Some(eyebrow) = span_text(line, EYEBROW_CLASS) {
            group = eyebrow.to_owned();
        }
        if !line.contains(CARD_MARKER) {
            continue;
        }
        let slug = attribute(line, HREF_ATTRIBUTE)
            .and_then(|href| href.strip_prefix(DOCS_PATH))
            .map(|slug| slug.trim_end_matches('/'));
        let (Some(slug), Some(title)) = (slug, span_text(line, CARD_TITLE_CLASS)) else {
            continue;
        };
        cards.push(Card {
            slug,
            title: title.to_owned(),
            description: span_text(line, CARD_DESCRIPTION_CLASS)
                .unwrap_or_default()
                .to_owned(),
            group: group.clone(),
        });
    }
    cards
}

fn span_text<'a>(line: &'a str, class: &str) -> Option<&'a str> {
    let open = format!("<span class=\"{class}\">");
    let start = line.find(&open)? + open.len();
    let end = line[start..].find("</span>")?;
    Some(&line[start..start + end])
}

fn attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let start = line.find(name)? + name.len();
    let end = line[start..].find('"')?;
    Some(&line[start..start + end])
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::Library;

    /// A library over the given `(slug, page)` pairs, each with a landing card, in the order given.
    pub(crate) fn library(pages: &[(&'static str, &'static str)]) -> Library {
        let landing: String = pages
            .iter()
            .map(|(slug, _)| {
                format!(
                    "<span class=\"eyebrow\">Group {slug}</span>\n<a class=\"card\" href=\"/docs/{slug}/\"><span class=\"card-title\">{slug}</span><span class=\"card-desc\">About {slug}.</span></a>\n"
                )
            })
            .collect();
        let files = pages
            .iter()
            .map(|(slug, text)| (leak(format!("{slug}/_index.md")), *text));
        Library::parse(std::iter::once(("_index.md", leak(landing))).chain(files))
    }

    pub(crate) fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }
}

#[cfg(test)]
pub(crate) mod site {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::LazyLock;

    use super::Library;
    use super::fixture::leak;

    const CONTENT_DIR: &str = "../site/docs/content";

    /// The real docs, read from disk so the tests always see the current files.
    pub(crate) fn library() -> &'static Library {
        static LIBRARY: LazyLock<Library> = LazyLock::new(|| Library::parse(files()));
        &LIBRARY
    }

    pub(crate) fn content_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(CONTENT_DIR)
    }

    fn files() -> Vec<(&'static str, &'static str)> {
        let root = content_dir();
        let mut files = Vec::new();
        collect(&root, &root, &mut files);
        files
    }

    fn collect(root: &Path, dir: &Path, files: &mut Vec<(&'static str, &'static str)>) {
        for entry in fs::read_dir(dir).expect("docs content directory") {
            let path = entry.expect("docs entry").path();
            if path.is_dir() {
                collect(root, &path, files);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                let relative = path.strip_prefix(root).expect("inside the content dir");
                files.push((
                    leak(relative.to_string_lossy().replace('\\', "/")),
                    leak(fs::read_to_string(&path).expect("docs page")),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::fixture::{leak, library};
    use super::{DocsError, LANDING_PAGE, SITE_DOCS_URL, Target, cards, site};

    const STATIC_DIR: &str = "../static";
    const ALPHA: &str = "+++\ntitle = \"Alpha\"\n+++\n\n# Alpha\n\nIntro text.\n\n## Shell timeout\n\nThe shell stops.\n\n### Details {#details}\n\nMore detail.\n\n```bash\n## Not a heading\n```\n\n## Tables\n\n| a | b |\n";
    const BETA: &str = "+++\ntitle = \"Beta\"\n+++\n\n# Beta\n\n## Labels\n\nLabels name things.\n";
    const GENERATED: &str = "# Gamma\n\n<!-- caudra-docgen:fields -->\n\n| field | meaning |\n\n<!-- /caudra-docgen:fields -->\n\n## After\n\nText.\n";
    const MARKER_NAME: &str = "docgen";
    const AFTER_HEADING: &str = "## After";
    const SEARCH_LIMIT: usize = 8;
    const MARKER_SHOWN: &str = "a generated region marker reached text a reader or the model sees";
    const HEADING_ADRIFT: &str = "a heading line no longer points at its heading in the display";

    fn pages() -> super::Library {
        library(&[("alpha", ALPHA), ("beta", BETA)])
    }

    #[test]
    fn headings_inside_code_fences_are_not_sections() {
        let library = pages();
        let anchors: Vec<&str> = library.pages()[0]
            .headings
            .iter()
            .map(|heading| heading.anchor.as_str())
            .collect();
        assert_eq!(anchors, ["alpha", "shell-timeout", "details", "tables"]);
    }

    #[test_case("/alpha#shell-timeout", "## Shell timeout\n\nThe shell stops.\n\n### Details {#details}\n\nMore detail.\n\n```bash\n## Not a heading\n```" ; "section with its subsection")]
    #[test_case("/alpha#details", "### Details {#details}\n\nMore detail.\n\n```bash\n## Not a heading\n```" ; "subsection")]
    #[test_case("/alpha#alpha", "# Alpha\n\nIntro text." ; "title anchor loads the intro")]
    #[test_case("/beta", "# Beta\n\n## Labels\n\nLabels name things." ; "whole page without front matter")]
    #[test_case("beta/#Labels", "## Labels\n\nLabels name things." ; "url shape and any case")]
    fn a_section_ends_at_the_next_heading_of_same_or_higher_level(address: &str, expected: &str) {
        assert_eq!(pages().load(address).as_deref(), Ok(expected));
    }

    #[test]
    fn generated_region_markers_stay_out_of_every_view() {
        let library = library(&[("gamma", GENERATED)]);
        let page = &library.pages()[0];
        let loaded = library.load("/gamma").expect("the page loads");
        for text in [loaded.as_str(), page.display.as_str()] {
            assert!(!text.contains(MARKER_NAME), "{MARKER_SHOWN}: {text}");
        }
        let search = library.search(MARKER_NAME, SEARCH_LIMIT);
        assert!(search.hits.is_empty(), "{MARKER_SHOWN}");
        let after = page.headings.last().expect("a section");
        assert_eq!(
            page.display.lines().nth(after.line),
            Some(AFTER_HEADING),
            "{HEADING_ADRIFT}"
        );
    }

    #[test]
    fn unknown_page_lists_the_pages() {
        let error = pages().load("/gamma").unwrap_err();
        assert_eq!(
            error,
            DocsError::UnknownPage {
                page: "gamma".into(),
                available: "alpha, beta".into()
            }
        );
    }

    #[test]
    fn unknown_section_lists_the_page_sections() {
        let error = pages().load("/alpha#nope").unwrap_err();
        assert_eq!(
            error,
            DocsError::UnknownSection {
                page: "alpha".into(),
                anchor: "nope".into(),
                available: "shell-timeout, tables".into()
            }
        );
    }

    #[test_case("beta", Some(Target { page: 1, heading: None }) ; "bare slug")]
    #[test_case("alpha#details", Some(Target { page: 0, heading: Some(2) }) ; "slug and section")]
    #[test_case("/docs/alpha/#tables", Some(Target { page: 0, heading: Some(3) }) ; "site path")]
    #[test_case("https://caudra.ai/docs/beta/#labels", Some(Target { page: 1, heading: Some(1) }) ; "site url")]
    #[test_case("alpha#missing", Some(Target { page: 0, heading: None }) ; "unknown section finds the page")]
    #[test_case("shell timeout", None ; "search words")]
    fn locate_accepts_slugs_sections_site_paths_and_site_urls(
        target: &str,
        expected: Option<Target>,
    ) {
        assert_eq!(pages().locate(target), expected);
    }

    #[test]
    fn every_landing_card_has_a_page_and_every_page_a_card() {
        let dir = site::content_dir();
        let mut on_disk: Vec<&str> = Vec::new();
        for entry in fs::read_dir(&dir).expect("content dir") {
            let path = entry.expect("content entry").path();
            if path.join(LANDING_PAGE).is_file() {
                on_disk.push(leak(
                    path.file_name()
                        .expect("page dir")
                        .to_string_lossy()
                        .into_owned(),
                ));
            }
        }
        on_disk.sort_unstable();
        let landing = fs::read_to_string(dir.join(LANDING_PAGE)).expect("landing page");
        let mut carded: Vec<&str> = cards(leak(landing)).iter().map(|card| card.slug).collect();
        carded.sort_unstable();
        assert_eq!(carded, on_disk);
    }

    #[test]
    fn every_page_starts_with_its_title() {
        for page in site::library().pages() {
            let first = page.headings.first().expect("page has headings");
            assert_eq!((page.slug, first.level, first.line), (page.slug, 1, 0));
        }
    }

    #[test]
    fn every_docs_link_resolves() {
        let library = site::library();
        let static_dir = site::content_dir().join(STATIC_DIR);
        let link_open = format!("]({SITE_DOCS_URL}");
        let mut broken = Vec::new();
        for page in library.pages() {
            let mut rest = page.display.as_str();
            while let Some(found) = rest.find(&link_open) {
                let target_start = found + 2;
                let end = rest[target_start..]
                    .find(')')
                    .map_or(rest.len(), |end| target_start + end);
                let target = &rest[target_start..end];
                let path = &target[SITE_DOCS_URL.len()..];
                if !static_dir.join(path).is_file() && library.resolve(path).is_err() {
                    broken.push(format!("{}: {target}", page.slug));
                }
                rest = &rest[end..];
            }
        }
        assert!(
            broken.is_empty(),
            "broken docs links:\n{}",
            broken.join("\n")
        );
    }
}
