//! Caudra's user documentation as data: the pages under `site/src/content/docs`, their sections and addresses, the
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
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::OnceLock;

use serde::Deserialize;
use thiserror::Error;

pub use search::{Correction, Hit, Search};

pub const NAME: &str = "caudra-docs";
pub const SKILL_DESCRIPTION: &str = "Caudra's own user documentation for this build: caudra.toml \
configuration, permissions, tools, commands, keybindings, providers, sessions, MCP, skills, plugins, workflows. \
Load it before answering questions about using or configuring Caudra.";
pub const SITE_DOCS_URL: &str = "https://caudra.ai/docs/";

const DOCS_PATH: &str = "/docs/";
const LANDING_PAGE: &str = "index";
const FRONT_MATTER_FENCE: &str = "---";
const NAVIGATION_FILE: &str = "docs-navigation.json";
const PAGE_TITLE_LEVEL: u8 = 1;
const SECTION_LEVEL: u8 = 2;

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

#[derive(Debug, Error, PartialEq, Eq)]
#[error("Invalid docs source `{path}`: {message}")]
pub struct SourceError {
    pub path: String,
    pub message: String,
}

impl SourceError {
    fn new(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            message: message.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    title: String,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Navigation {
    groups: Vec<Group>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    label: String,
    pages: Vec<String>,
}

impl Library {
    /// Validates flat `<slug>.md` sources with YAML metadata and the shared navigation manifest.
    /// The overview is first, followed by every other page in navigation order.
    pub fn parse(
        files: impl IntoIterator<Item = (&'static str, &'static str)>,
        navigation: &str,
    ) -> Result<Self, SourceError> {
        let navigation: Navigation = serde_json::from_str(navigation)
            .map_err(|error| SourceError::new(NAVIGATION_FILE, error.to_string()))?;
        let mut sources = BTreeMap::new();
        for (path, text) in files {
            let slug = page_slug(path).ok_or_else(|| {
                SourceError::new(path, "expected a flat lowercase <slug>.md filename")
            })?;
            let page = Page::parse(slug, text)?;
            if sources.insert(slug, page).is_some() {
                return Err(SourceError::new(path, "duplicate page source"));
            }
        }
        let mut overview = sources
            .remove(LANDING_PAGE)
            .ok_or_else(|| SourceError::new("index.md", "missing overview page"))?;
        overview.group = "Overview".to_owned();
        let mut pages = vec![overview];
        let mut listed = BTreeSet::new();
        let mut labels = BTreeSet::new();
        for group in navigation.groups {
            if group.label.trim().is_empty() || group.pages.is_empty() {
                return Err(SourceError::new(
                    NAVIGATION_FILE,
                    "each group needs a nonempty label and pages",
                ));
            }
            if !labels.insert(group.label.clone()) {
                return Err(SourceError::new(
                    NAVIGATION_FILE,
                    format!("duplicate group label `{}`", group.label),
                ));
            }
            for slug in group.pages {
                if slug == LANDING_PAGE || !listed.insert(slug.clone()) {
                    return Err(SourceError::new(
                        NAVIGATION_FILE,
                        format!(
                            "page `{slug}` must be listed exactly once; index must not be listed"
                        ),
                    ));
                }
                let mut page = sources.remove(slug.as_str()).ok_or_else(|| {
                    SourceError::new(
                        NAVIGATION_FILE,
                        format!(
                            "group `{}` references missing page `{slug}.md`",
                            group.label
                        ),
                    )
                })?;
                page.group.clone_from(&group.label);
                pages.push(page);
            }
        }
        if !sources.is_empty() {
            return Err(SourceError::new(
                NAVIGATION_FILE,
                format!(
                    "add unlisted pages to a group: {}",
                    sources.keys().copied().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        Ok(Self {
            pages,
            search: OnceLock::new(),
        })
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
        let slug = if slug.is_empty() { LANDING_PAGE } else { slug };
        self.pages
            .iter()
            .position(|page| page.slug.eq_ignore_ascii_case(slug))
    }
}

impl Page {
    fn parse(slug: &'static str, text: &'static str) -> Result<Self, SourceError> {
        let path = format!("{slug}.md");
        let (metadata, text) = metadata(&path, text)?;
        let text = markdown::without_comments(text);
        if markdown::headings(&text)
            .iter()
            .any(|heading| heading.level == PAGE_TITLE_LEVEL)
        {
            return Err(SourceError::new(
                &path,
                "put the page title in frontmatter, not a body H1",
            ));
        }
        let body = Cow::Owned(format!("# {}\n\n{}", metadata.title, text.trim_start()));
        let headings = markdown::headings(&body);
        Ok(Self {
            slug,
            title: metadata.title,
            description: metadata.description,
            group: String::new(),
            display: display::render(&body, slug),
            line_starts: line_starts(&body),
            body,
            headings,
        })
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
    let slug = path.strip_suffix(".md")?;
    (!slug.is_empty()
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
    .then_some(slug)
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

fn metadata<'a>(path: &str, text: &'a str) -> Result<(Metadata, &'a str), SourceError> {
    let mut lines = text.split_inclusive('\n');
    let opening = lines.next().unwrap_or_default();
    if opening.trim_end() != FRONT_MATTER_FENCE {
        return Err(SourceError::new(
            path,
            "start the page with YAML frontmatter fenced by ---",
        ));
    }
    let start = opening.len();
    let mut end = start;
    for line in lines {
        if line.trim_end() == FRONT_MATTER_FENCE {
            let metadata: Metadata = serde_yaml::from_str(&text[start..end])
                .map_err(|error| SourceError::new(path, format!("YAML frontmatter: {error}")))?;
            for (name, value) in [
                ("title", &metadata.title),
                ("description", &metadata.description),
            ] {
                if value.trim().is_empty() || value.contains(['\r', '\n']) {
                    return Err(SourceError::new(
                        path,
                        format!("`{name}` must be a nonempty single line"),
                    ));
                }
            }
            return Ok((metadata, &text[end + line.len()..]));
        }
        end += line.len();
    }
    Err(SourceError::new(
        path,
        "close YAML frontmatter with a line containing ---",
    ))
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::Library;

    /// A library over Markdown snippets, in the order given, without an overview fixture.
    pub(crate) fn library(pages: &[(&'static str, &'static str)]) -> Library {
        Library {
            pages: pages
                .iter()
                .map(|(slug, text)| {
                    let (title, body) = text
                        .strip_prefix("# ")
                        .expect("fixture title")
                        .split_once('\n')
                        .unwrap();
                    let source = leak(format!(
                        "---\ntitle: {}\ndescription: About {slug}.\n---\n{body}",
                        serde_json::to_string(title).unwrap()
                    ));
                    let mut page = super::Page::parse(slug, source).unwrap();
                    page.group = format!("Group {slug}");
                    page
                })
                .collect(),
            search: Default::default(),
        }
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

    const CONTENT_DIR: &str = "../site/src/content/docs";

    /// The real docs, read from disk so the tests always see the current files.
    pub(crate) fn library() -> &'static Library {
        static LIBRARY: LazyLock<Library> = LazyLock::new(|| {
            let navigation =
                fs::read_to_string(content_dir().join("../../data/docs-navigation.json"))
                    .expect("docs navigation");
            Library::parse(files(), &navigation).expect("valid docs sources")
        });
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
    use super::{DocsError, Library, NAVIGATION_FILE, Page, SITE_DOCS_URL, Target, site};

    const STATIC_DIR: &str = "../../../public/docs";
    const ALPHA: &str = "# Alpha\n\nIntro text.\n\n## Shell timeout\n\nThe shell stops.\n\n### Details {#details}\n\nMore detail.\n\n```bash\n## Not a heading\n```\n\n## Tables\n\n| a | b |\n";
    const BETA: &str = "# Beta\n\n## Labels\n\nLabels name things.\n";
    const GENERATED: &str = "# Gamma\n\n<!-- caudra-docgen:fields -->\n\n| field | meaning |\n\n<!-- /caudra-docgen:fields -->\n\n## After\n\nText.\n";
    const MARKER_NAME: &str = "docgen";
    const AFTER_HEADING: &str = "## After";
    const SEARCH_LIMIT: usize = 8;
    const MARKER_SHOWN: &str = "a generated region marker reached text a reader or the model sees";
    const HEADING_ADRIFT: &str = "a heading line no longer points at its heading in the display";
    const OVERVIEW: &str = "---\ntitle: Overview\ndescription: metadataonly\n---\n\nIntroductory wombats.\n\n## Detail {#stable}\n\nBody.\n";
    const VALID_NAVIGATION: &str = r#"{"groups":[{"label":"Guides","pages":["alpha"]}]}"#;
    const UNLISTED: &str = "add unlisted pages to a group: alpha";
    const REPEATED: &str = "page `alpha` must be listed exactly once; index must not be listed";
    const LISTED_INDEX: &str = "page `index` must be listed exactly once; index must not be listed";
    const MISSING_PAGE: &str = "group `Guides` references missing page `missing.md`";
    const EMPTY_GROUP: &str = "each group needs a nonempty label and pages";

    fn pages() -> super::Library {
        library(&[("alpha", ALPHA), ("beta", BETA)])
    }

    #[test_case(r#"{"groups":[]}"#, UNLISTED ; "unlisted_page")]
    #[test_case(r#"{"groups":[{"label":"Guides","pages":["alpha","alpha"]}]}"#, REPEATED ; "duplicate_page")]
    #[test_case(r#"{"groups":[{"label":"Guides","pages":["index","alpha"]}]}"#, LISTED_INDEX ; "listed_overview")]
    #[test_case(r#"{"groups":[{"label":"Guides","pages":["missing"]}]}"#, MISSING_PAGE ; "missing_page")]
    #[test_case(r#"{"groups":[{"label":"","pages":["alpha"]}]}"#, EMPTY_GROUP ; "empty_label")]
    #[test_case(r#"{"groups":[{"label":"Guides","pages":[]}]}"#, EMPTY_GROUP ; "empty_group")]
    fn invalid_navigation_names_the_problem(navigation: &str, expected: &str) {
        let error = Library::parse([("index.md", OVERVIEW), ("alpha.md", OVERVIEW)], navigation)
            .err()
            .expect("invalid navigation");
        assert_eq!(error.path, NAVIGATION_FILE);
        assert_eq!(error.message, expected);
    }

    #[test_case("# Old title\n", "start the page" ; "missing_yaml")]
    #[test_case("---\ntitle: Title\n", "close YAML" ; "missing_fence")]
    #[test_case("---\ntitle: Title\n---\n", "missing field `description`" ; "missing_description")]
    #[test_case("---\ntitle: ''\ndescription: Description\n---\n", "`title` must be" ; "empty_title")]
    #[test_case("---\ntitle: Title\ndescription: Description\nweight: 2\n---\n", "unknown field `weight`" ; "legacy_metadata")]
    #[test_case("---\ntitle: Title\ndescription: Description\n---\n# Duplicate\n", "not a body H1" ; "duplicate_title")]
    fn invalid_metadata_names_the_page(source: &'static str, expected: &str) {
        let error = Page::parse("invalid", source).unwrap_err();
        assert_eq!(error.path, "invalid.md");
        assert!(error.message.contains(expected), "{error}");
    }

    #[test]
    fn metadata_is_not_body_text_and_introductions_are_searchable() {
        let library = Library::parse(
            [("alpha.md", OVERVIEW), ("index.md", OVERVIEW)],
            VALID_NAVIGATION,
        )
        .unwrap();
        assert_eq!(
            library
                .pages()
                .iter()
                .map(|page| page.slug)
                .collect::<Vec<_>>(),
            ["index", "alpha"]
        );
        for page in library.pages() {
            assert!(page.body.starts_with("# Overview\n\nIntroductory wombats."));
            assert!(!page.body.contains("metadataonly"));
            assert!(!page.display.contains("description:"));
            assert_eq!(page.headings[1].anchor, "stable");
        }
        assert!(library.search("metadataonly", SEARCH_LIMIT).hits.is_empty());
        let search = library.search("wombats", SEARCH_LIMIT);
        assert_eq!(search.hits.len(), 2);
        assert!(search.hits.iter().all(|hit| hit.heading == 0));
        assert_eq!(
            library.locate("/docs/"),
            Some(Target {
                page: 0,
                heading: None
            })
        );
        assert!(library.load("/index#overview").unwrap().contains("wombats"));
        assert!(library.index().contains("metadataonly"));
    }

    #[test]
    fn canonical_overview_is_searchable() {
        let library = site::library();
        let search = library.search("effective action", SEARCH_LIMIT);
        assert!(
            search
                .hits
                .iter()
                .any(|hit| library.pages()[hit.page].slug == "index")
        );
        assert!(library.load("/index").unwrap().contains("independent fork"));
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
    fn every_source_is_in_the_validated_library() {
        let dir = site::content_dir();
        let mut on_disk: Vec<&str> = Vec::new();
        for entry in fs::read_dir(&dir).expect("content dir") {
            let path = entry.expect("content entry").path();
            if path.extension().is_some_and(|extension| extension == "md") {
                on_disk.push(leak(
                    path.file_stem()
                        .expect("page slug")
                        .to_string_lossy()
                        .into_owned(),
                ));
            }
        }
        on_disk.sort_unstable();
        let mut parsed: Vec<&str> = site::library()
            .pages()
            .iter()
            .map(|page| page.slug)
            .collect();
        parsed.sort_unstable();
        assert_eq!(parsed, on_disk);
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
