//! A vendored skill pack served as the pages of one builtin skill: every file by its path, a skill
//! by its name, and a word search. `upstream/` is the copy `make sync-<pack>` writes, and
//! `translated/` replaces each file that names another harness's vocabulary with its Caudra
//! translation (`.caudra/skills/skillpack-translate`).

#![forbid(unsafe_code)]

#[cfg(feature = "checks")]
pub mod checks;

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use caudra_docs::{SearchIndex, SearchReport};
use include_dir::{Dir, File};
use serde::Deserialize;

pub const MANIFEST: &str = "upstream.json";
pub const TRANSLATIONS: &str = "translations.json";
pub const SKILLS_DIR: &str = "skills";
pub const SKILL_FILE: &str = "SKILL.md";
pub const MARKDOWN: &str = ".md";
const README_FILE: &str = "README.md";
const SEARCH_SEPARATOR: char = '?';
const PATH_SEPARATOR: char = '/';
const PARENT: &str = "..";
const CURRENT: &str = ".";
pub const SEARCH_LIMIT: usize = 8;
const SEARCH_UNIT: &str = "page";
/// The heading the search index puts above each page: its path.
const PATH_HEADING: usize = 0;
const SUGGESTION_LIMIT: usize = 5;
const SHORT_COMMIT: usize = 9;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Upstream {
    pub repository: String,
    pub commit: String,
    #[serde(default)]
    pub version: String,
}

impl Upstream {
    /// The manifest `make sync-<pack>` writes beside the vendored files.
    pub fn read(tree: &Dir<'_>) -> Self {
        tree.get_file(MANIFEST)
            .and_then(File::contents_utf8)
            .and_then(|manifest| serde_json::from_str(manifest).ok())
            .unwrap_or_default()
    }

    pub fn short_commit(&self) -> String {
        self.commit.chars().take(SHORT_COMMIT).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// Upstream's `disable-model-invocation`: run only when the user asks for it by name.
    pub user_only: bool,
}

impl Skill {
    pub fn summary(&self) -> &str {
        first_sentence(&self.description)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "argument-hint")]
    pub argument_hint: Option<String>,
    #[serde(rename = "disable-model-invocation", default)]
    pub disable_model_invocation: bool,
}

/// What an address loaded, and the raw text when it named a page rather than a listing or a
/// search, so a pack can append what that page points at.
#[derive(Debug)]
pub struct Loaded {
    pub text: String,
    pub page: Option<&'static str>,
}

pub struct Library {
    name: &'static str,
    pages: BTreeMap<String, &'static str>,
    upstream: Upstream,
    skills: Vec<Skill>,
    search: OnceLock<SearchIndex>,
}

impl Library {
    /// `name` is the skill's name and the prefix of every address; `pages` maps each served path
    /// to its text, a skill at `skills/<name>/SKILL.md`.
    pub fn new(
        name: &'static str,
        pages: BTreeMap<String, &'static str>,
        upstream: Upstream,
    ) -> Self {
        let skills = pages
            .iter()
            .filter_map(|(path, text)| {
                let directory = skill_of(path)?;
                let front = frontmatter(text);
                Some(Skill {
                    name: front.name.unwrap_or_else(|| directory.to_owned()),
                    description: front.description.unwrap_or_default().trim().to_owned(),
                    argument_hint: front.argument_hint,
                    user_only: front.disable_model_invocation,
                })
            })
            .collect();
        Self {
            name,
            pages,
            upstream,
            skills,
            search: OnceLock::new(),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn pages(&self) -> &BTreeMap<String, &'static str> {
        &self.pages
    }

    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// The index line of one skill.
    pub fn entry(&self, skill: &Skill) -> String {
        format!("- `{}/{}`: {}\n", self.name, skill.name, skill.summary())
    }

    /// One page by the address that follows the pack's name: `/path`, or `?words` to search.
    pub fn load(&self, address: &str) -> Result<Loaded, String> {
        let name = self.name;
        if let Some(words) = address.strip_prefix(SEARCH_SEPARATOR) {
            return Ok(Loaded {
                text: self.search(words),
                page: None,
            });
        }
        let requested = address.trim_start_matches(PATH_SEPARATOR);
        let path = normalize(requested).ok_or_else(|| {
            format!("`{name}/{requested}` leaves {name}; addresses stay inside the plugin")
        })?;
        if let Some((found, text)) = self.page(&path) {
            return Ok(Loaded {
                text: self.headed(&found, text),
                page: Some(text),
            });
        }
        if let Some(listing) = self.listing(&path) {
            return Ok(Loaded {
                text: listing,
                page: None,
            });
        }
        Err(self.not_found(&path))
    }

    /// The page an address names: the exact file, a directory's `SKILL.md` or `README.md`, or the
    /// same under `skills/`.
    pub fn page(&self, path: &str) -> Option<(String, &'static str)> {
        let under_skills = format!("{SKILLS_DIR}/{path}");
        [path, under_skills.as_str()]
            .into_iter()
            .flat_map(|base| {
                [
                    base.to_owned(),
                    format!("{base}/{SKILL_FILE}"),
                    format!("{base}/{README_FILE}"),
                ]
            })
            .find_map(|candidate| {
                let text = self.pages.get(&candidate)?;
                Some((candidate, *text))
            })
    }

    pub fn listing(&self, path: &str) -> Option<String> {
        let prefixes = match path.is_empty() {
            true => vec![String::new()],
            false => vec![format!("{path}/"), format!("{SKILLS_DIR}/{path}/")],
        };
        prefixes.into_iter().find_map(|prefix| {
            let entries: Vec<String> = self
                .pages
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .map(|key| format!("- `{}/{key}`", self.name))
                .collect();
            (!entries.is_empty())
                .then(|| format!("`{}/{prefix}` holds:\n\n{}", self.name, entries.join("\n")))
        })
    }

    /// The pages that contain every word, ranked as `caudra-docs?<words>` ranks sections, each by
    /// its best section.
    fn search(&self, words: &str) -> String {
        let name = self.name;
        let query = words.trim();
        if query.is_empty() {
            return format!("Search with `{name}?<words>`, such as `{name}?flaky test`.");
        }
        let mut search = self.search_index().search(query, usize::MAX);
        let mut pages = HashSet::new();
        search.hits.retain(|hit| pages.insert(hit.page));
        search.matched = search.hits.len();
        search.hits.truncate(SEARCH_LIMIT);
        let no_match = format!("No {name} page contains every word of `{query}`.");
        let mut report = SearchReport::new(&search, query, name, SEARCH_UNIT, &no_match);
        for hit in &search.hits {
            let Some((path, text)) = self.pages.iter().nth(hit.page) else {
                continue;
            };
            let place = (hit.heading != PATH_HEADING).then_some(hit.title.as_str());
            report.entry(&format!("{name}/{path}"), place, text.len(), &hit.snippet);
        }
        report.finish()
    }

    /// Every page in path order under a heading naming its path, so a page without headings of
    /// its own, such as a script, is searched too.
    fn search_index(&self) -> &SearchIndex {
        self.search.get_or_init(|| {
            let bodies: Vec<String> = self
                .pages
                .iter()
                .map(|(path, text)| format!("# {path}\n\n{}", body(text)))
                .collect();
            SearchIndex::from_markdown(bodies.iter().map(String::as_str))
        })
    }

    fn not_found(&self, path: &str) -> String {
        let name = self.name;
        let last = path.rsplit(PATH_SEPARATOR).next().unwrap_or(path);
        let mut near: Vec<&String> = self
            .pages
            .keys()
            .filter(|key| !last.is_empty() && key.contains(last))
            .take(SUGGESTION_LIMIT)
            .collect();
        if near.is_empty() {
            let mut ranked: Vec<(usize, &String)> = self
                .pages
                .keys()
                .map(|key| (distance(last, key_stem(key)), key))
                .collect();
            ranked.sort();
            near = ranked
                .into_iter()
                .take(SUGGESTION_LIMIT)
                .map(|(_, key)| key)
                .collect();
        }
        let near: Vec<String> = near.iter().map(|key| format!("`{name}/{key}`")).collect();
        format!(
            "{name} has no page `{name}/{path}`. Closest: {}. Load `{name}` for the index.",
            near.join(", ")
        )
    }

    fn headed(&self, path: &str, text: &str) -> String {
        let name = self.name;
        let dir = path.rsplit_once(PATH_SEPARATOR).map_or("", |(dir, _)| dir);
        format!(
            "Page `{name}/{path}`. A relative path in it resolves against `{name}/{dir}/`.\n\n{text}"
        )
    }
}

/// Every file under `dir`, at any depth.
pub fn files(dir: &'static Dir<'static>) -> Vec<&'static File<'static>> {
    let mut found: Vec<&'static File<'static>> = dir.files().collect();
    for child in dir.dirs() {
        found.extend(files(child));
    }
    found
}

/// A file's path in its tree and its text, `None` for a binary file.
pub fn text_file(file: &'static File<'static>) -> Option<(&'static str, &'static str)> {
    Some((file.path().to_str()?, file.contents_utf8()?))
}

/// `skills/<name>/SKILL.md`'s name.
pub fn skill_of(path: &str) -> Option<&str> {
    let rest = path
        .strip_prefix(SKILLS_DIR)?
        .strip_prefix(PATH_SEPARATOR)?;
    let (name, file) = rest.split_once(PATH_SEPARATOR)?;
    (file == SKILL_FILE).then_some(name)
}

fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    match rest.find("\n---") {
        Some(end) => {
            let after = &rest[end + 4..];
            (Some(&rest[..end]), after.trim_start_matches('\n'))
        }
        None => (None, text),
    }
}

pub fn frontmatter(text: &str) -> Frontmatter {
    split_frontmatter(text)
        .0
        .and_then(|yaml| serde_yaml::from_str(yaml).ok())
        .unwrap_or_default()
}

pub fn body(text: &str) -> &str {
    split_frontmatter(text).1
}

/// Resolves `.` and `..` and drops empty segments; `None` when the path climbs out of the pack.
pub fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(PATH_SEPARATOR) {
        match part {
            "" | CURRENT => {}
            PARENT => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

pub fn first_sentence(text: &str) -> &str {
    text.find(". ").map_or(text, |end| &text[..=end])
}

fn key_stem(key: &str) -> &str {
    let key = key
        .strip_suffix(SKILL_FILE)
        .unwrap_or(key)
        .trim_end_matches(PATH_SEPARATOR);
    key.rsplit(PATH_SEPARATOR).next().unwrap_or(key)
}

fn distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = (previous + usize::from(a != *b))
                .min(row[j] + 1)
                .min(current + 1);
            previous = current;
        }
    }
    row[right.len()]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const PACK: &str = "pack";
    const TDD: &str = "skills/tdd/SKILL.md";
    const TDD_TEXT: &str = "---\nname: tdd\ndescription: Test first. Then the rest.\nargument-hint: \"What to build\"\n---\n\nRed, green.\n";
    const SPEC: &str = "skills/to-spec/SKILL.md";
    const SPEC_TEXT: &str = "---\nname: to-spec\ndescription: Write a spec.\ndisable-model-invocation: true\n---\n\nSpec.\n";
    const NOTE: &str = "skills/tdd/notes/mocking.md";
    const NOTE_TEXT: &str = "# Mocking\n\nMock at the seams.\n";

    fn library() -> Library {
        Library::new(
            PACK,
            BTreeMap::from([
                (TDD.to_owned(), TDD_TEXT),
                (SPEC.to_owned(), SPEC_TEXT),
                (NOTE.to_owned(), NOTE_TEXT),
            ]),
            Upstream::default(),
        )
    }

    #[test]
    fn frontmatter_sets_the_hint_and_who_may_run_a_skill() {
        let library = library();
        let tdd = library.skill("tdd").unwrap();
        assert_eq!(tdd.argument_hint.as_deref(), Some("What to build"));
        assert_eq!(tdd.summary(), "Test first.");
        assert!(!tdd.user_only);
        assert!(library.skill("to-spec").unwrap().user_only);
    }

    #[test_case("/tdd", TDD ; "skill_shorthand")]
    #[test_case("/skills/tdd/SKILL.md", TDD ; "full_path")]
    #[test_case("/tdd/notes/mocking.md", NOTE ; "skill_relative_file")]
    #[test_case("/skills/to-spec/../tdd", TDD ; "parent_segment")]
    fn an_address_loads_its_page(address: &str, path: &str) {
        let loaded = library().load(address).unwrap();
        assert!(
            loaded.text.starts_with(&format!("Page `{PACK}/{path}`")),
            "{}",
            loaded.text
        );
        assert!(loaded.page.is_some());
    }

    #[test]
    fn an_address_cannot_climb_out_of_the_pack() {
        assert!(library().load("/../../etc/passwd").is_err());
    }

    #[test]
    fn an_unknown_address_names_the_nearest_pages() {
        let error = library().load("/td").unwrap_err();
        assert!(error.contains("`pack/skills/tdd/SKILL.md`"), "{error}");
    }

    #[test]
    fn a_directory_without_an_index_lists_its_pages() {
        let loaded = library().load("/tdd/notes").unwrap();
        assert!(loaded.text.contains("`pack/skills/tdd/notes/mocking.md`"));
        assert!(loaded.page.is_none());
    }

    #[test]
    fn a_search_finds_the_page_with_every_word() {
        let loaded = library().load("?mock seams").unwrap();
        assert!(
            loaded.text.contains("pack/skills/tdd/notes/mocking.md"),
            "{}",
            loaded.text
        );
    }
}
