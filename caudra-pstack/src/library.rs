//! The vendored pstack as pages: every file by its path in the plugin, the shorthands the skills'
//! own routing uses, an index the `pstack` skill opens with, and a word search.

use std::collections::{BTreeMap, HashSet};
use std::sync::{LazyLock, OnceLock};

use caudra_docs::{SearchIndex, SearchReport};
use include_dir::{Dir, File, include_dir};
use serde::Deserialize;

use crate::roles::{MODEL_ROLES_REF, RoleTable};

static UPSTREAM: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/upstream");
static TRANSLATED: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/translated");
static AUTOMATIONS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/automations");
static LIBRARY: LazyLock<Library> = LazyLock::new(Library::embedded);

/// The skill's name, and the prefix of every address.
pub const NAME: &str = "pstack";
pub const SKILL_DESCRIPTION: &str = "poteto's pstack: rigorous engineering skills (poteto-mode, \
     bug fixes, plans, reviews, orchestration). Load only when the user asks for pstack, poteto, \
     or one of its skills, or sends a `/pstack:<skill>` command; the index lists every skill.";
const PSTACK_ROOT: &str = "pstack";
const TEAM_KIT_ROOT: &str = "cursor-team-kit";
const MANIFEST: &str = "upstream.json";
const SKILLS_DIR: &str = "skills";
const AGENTS_DIR: &str = "agents";
const SKILL_FILE: &str = "SKILL.md";
const README_FILE: &str = "README.md";
const MARKDOWN: &str = ".md";
const PRINCIPLE_PREFIX: &str = "principle-";
const SEARCH_SEPARATOR: char = '?';
const PATH_SEPARATOR: char = '/';
const PARENT: &str = "..";
const CURRENT: &str = ".";
const SEARCH_LIMIT: usize = 8;
const SEARCH_UNIT: &str = "page";
/// The heading the search index puts above each page: its path.
const PATH_HEADING: usize = 0;
const SUGGESTION_LIMIT: usize = 5;
/// Entry points the index lists first, in this order.
const START_HERE: &[&str] = &["poteto-mode", "poteto-help"];
/// Upstream files Caudra replaces with a feature of its own, by their path in the vendored tree:
/// never served, indexed, or searched, and a retired skill has no command.
const RETIRED: &[&str] = &["pstack/skills/setup-pstack/SKILL.md"];
const GUIDE: &str = "docs/guide/README.md";
const BENNY: &str = "automations/benny/README.md";
/// Where Caudra's ports of pstack's automations sit among the pages.
const CAUDRA_AUTOMATIONS_DIR: &str = "caudra/automations";
const AUTOMATION_EXTENSION: &str = "rhai";

#[derive(Debug, Clone, Deserialize)]
pub struct Upstream {
    pub repository: String,
    pub commit: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// One of the `cursor-team-kit` skills pstack routes to, bundled beside it.
    pub team_kit: bool,
}

impl Skill {
    pub fn is_principle(&self) -> bool {
        self.name.starts_with(PRINCIPLE_PREFIX)
    }

    pub fn summary(&self) -> &str {
        first_sentence(&self.description)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub body: String,
}

#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

pub struct Library {
    /// Every page by its path in the plugin, the team-kit skills under `skills/` beside pstack's.
    pages: BTreeMap<String, &'static str>,
    upstream: Upstream,
    skills: Vec<Skill>,
    agents: Vec<Agent>,
    search: OnceLock<SearchIndex>,
}

pub fn library() -> &'static Library {
    &LIBRARY
}

/// The Caudra automations pstack ships, as `(stem, source)`: listed in `/automations` while pstack
/// is on, and served as pages under `pstack/caudra/automations`.
pub fn automations() -> impl Iterator<Item = (&'static str, &'static str)> {
    AUTOMATIONS
        .files()
        .filter(|file| {
            file.path()
                .extension()
                .is_some_and(|extension| extension == AUTOMATION_EXTENSION)
        })
        .filter_map(|file| Some((file.path().file_stem()?.to_str()?, file.contents_utf8()?)))
}

impl Library {
    fn embedded() -> Self {
        let mut pages = BTreeMap::new();
        for tree in [&UPSTREAM, &TRANSLATED] {
            for (path, text) in vendored(tree) {
                pages.insert(page_key(path), text);
            }
        }
        for (path, text) in files(&AUTOMATIONS).into_iter().filter_map(text_file) {
            pages.insert(format!("{CAUDRA_AUTOMATIONS_DIR}/{path}"), text);
        }
        let manifest = UPSTREAM
            .get_file(MANIFEST)
            .and_then(File::contents_utf8)
            .unwrap_or_default();
        let upstream = serde_json::from_str(manifest).unwrap_or_else(|_| Upstream {
            repository: String::new(),
            commit: String::new(),
            version: String::new(),
        });
        Self::new(pages, upstream)
    }

    fn new(pages: BTreeMap<String, &'static str>, upstream: Upstream) -> Self {
        let team_kit: Vec<String> = UPSTREAM
            .get_dir(TEAM_KIT_ROOT)
            .into_iter()
            .flat_map(files)
            .filter_map(|file| file.path().to_str())
            .filter_map(skill_of)
            .map(str::to_owned)
            .collect();
        let skills = pages
            .iter()
            .filter_map(|(path, text)| {
                let name = skill_of(path)?;
                let front = frontmatter(text);
                Some(Skill {
                    name: front.name.unwrap_or_else(|| name.to_owned()),
                    description: front.description.unwrap_or_default().trim().to_owned(),
                    team_kit: team_kit.iter().any(|kit| kit == name),
                })
            })
            .collect();
        let agents = pages
            .iter()
            .filter_map(|(path, text)| {
                let stem = path
                    .strip_prefix(AGENTS_DIR)?
                    .strip_prefix(PATH_SEPARATOR)?
                    .strip_suffix(MARKDOWN)?;
                let front = frontmatter(text);
                Some(Agent {
                    name: front.name.unwrap_or_else(|| stem.to_owned()),
                    description: front.description.unwrap_or_default().trim().to_owned(),
                    body: body(text).to_owned(),
                })
            })
            .collect();
        Self {
            pages,
            upstream,
            skills,
            agents,
            search: OnceLock::new(),
        }
    }

    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// What loading `pstack` itself returns: how to read the rest, the entry points, every
    /// skill, and the user's model roles.
    pub fn index(&self, roles: &RoleTable) -> String {
        let Upstream {
            version, commit, ..
        } = &self.upstream;
        let short: String = commit.chars().take(9).collect();
        let mut index = format!(
            "# pstack {version}\n\n\
             poteto's skills for rigorous engineering, bundled from cursor/plugins at `{short}`. \
             Every file is a page of this skill: load `{NAME}/<skill>` for a skill, or \
             `{NAME}/<path>` for any file by its path in the plugin. `{NAME}?<words>` searches \
             every page.\n\n\
             Run a skill only when the user asks for pstack or one of its skills, or sends a \
             `/{NAME}:<skill>` command.\n\n## Start here\n\n"
        );
        for name in START_HERE {
            if let Some(skill) = self.skill(name) {
                index.push_str(&entry(skill));
            }
        }
        index.push_str(&format!(
            "- `{NAME}/{GUIDE}`: the pstack guide, a first real task from setup to overnight runs.\n\n## Skills\n\n"
        ));
        let listed = self
            .skills
            .iter()
            .filter(|skill| !skill.is_principle() && !START_HERE.contains(&skill.name.as_str()));
        for skill in listed {
            index.push_str(&entry(skill));
        }
        let principles: Vec<String> = self
            .skills
            .iter()
            .filter(|skill| skill.is_principle())
            .map(|skill| format!("`{NAME}/{}`", skill.name))
            .collect();
        index.push_str(&format!(
            "\n## Principles\n\n`poteto-mode` says when each applies. {}\n\n## Agents and automations\n\n",
            principles.join(", ")
        ));
        for agent in &self.agents {
            index.push_str(&format!(
                "- `{NAME}/{AGENTS_DIR}/{}{MARKDOWN}`, the `{NAME}-{}` task profile: {}\n",
                agent.name,
                agent.name,
                first_sentence(&agent.description)
            ));
        }
        index.push_str(&format!(
            "- `{NAME}/{BENNY}`: benny, triage and reproduction of Slack issue reports.\n\
             - `{NAME}/{CAUDRA_AUTOMATIONS_DIR}`: the source of benny's Slack polling and \
             watch-pr's drive mode as Caudra automations. They are bundled: with \
             `experimental.automations` on they are listed in `/automations`, where the user arms \
             them; a project or user script of the same name overrides one.\n\n{}",
            roles.render()
        ));
        index
    }

    /// One page by the address that follows `pstack`: `/path`, or `?words` to search. A page that
    /// points at the model roles ends with `roles`.
    pub fn load(&self, address: &str, roles: &RoleTable) -> Result<String, String> {
        if let Some(words) = address.strip_prefix(SEARCH_SEPARATOR) {
            return Ok(self.search(words));
        }
        let requested = address.trim_start_matches(PATH_SEPARATOR);
        let path = normalize(requested).ok_or_else(|| {
            format!("`{NAME}/{requested}` leaves pstack; addresses stay inside the plugin")
        })?;
        if let Some((found, text)) = self.page(&path) {
            let mut page = headed(&found, text);
            if text.contains(MODEL_ROLES_REF) {
                page.push_str("\n\n");
                page.push_str(&roles.render());
            }
            return Ok(page);
        }
        if let Some(listing) = self.listing(&path) {
            return Ok(listing);
        }
        Err(self.not_found(&path))
    }

    /// The page an address names: the exact file, a directory's `SKILL.md` or `README.md`, or the
    /// same under `skills/`.
    fn page(&self, path: &str) -> Option<(String, &'static str)> {
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

    fn listing(&self, path: &str) -> Option<String> {
        let prefixes = match path.is_empty() {
            true => vec![String::new()],
            false => vec![format!("{path}/"), format!("{SKILLS_DIR}/{path}/")],
        };
        prefixes.into_iter().find_map(|prefix| {
            let entries: Vec<String> = self
                .pages
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .map(|key| format!("- `{NAME}/{key}`"))
                .collect();
            (!entries.is_empty())
                .then(|| format!("`{NAME}/{prefix}` holds:\n\n{}", entries.join("\n")))
        })
    }

    /// The pages that contain every word, ranked as `caudra-docs?<words>` ranks sections, each by
    /// its best section.
    fn search(&self, words: &str) -> String {
        let query = words.trim();
        if query.is_empty() {
            return format!("Search with `{NAME}?<words>`, such as `{NAME}?flaky test`.");
        }
        let mut search = self.search_index().search(query, usize::MAX);
        let mut pages = HashSet::new();
        search.hits.retain(|hit| pages.insert(hit.page));
        search.matched = search.hits.len();
        search.hits.truncate(SEARCH_LIMIT);
        let no_match = format!("No pstack page contains every word of `{query}`.");
        let mut report = SearchReport::new(&search, query, NAME, SEARCH_UNIT, &no_match);
        for hit in &search.hits {
            let Some((path, text)) = self.pages.iter().nth(hit.page) else {
                continue;
            };
            let place = (hit.heading != PATH_HEADING).then_some(hit.title.as_str());
            report.entry(&format!("{NAME}/{path}"), place, text.len(), &hit.snippet);
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
        let near: Vec<String> = near.iter().map(|key| format!("`{NAME}/{key}`")).collect();
        format!(
            "pstack has no page `{NAME}/{path}`. Closest: {}. Load `{NAME}` for the index.",
            near.join(", ")
        )
    }

    fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }
}

fn files(dir: &'static Dir<'static>) -> Vec<&'static File<'static>> {
    let mut found: Vec<&'static File<'static>> = dir.files().collect();
    for child in dir.dirs() {
        found.extend(files(child));
    }
    found
}

fn text_file(file: &'static File<'static>) -> Option<(&'static str, &'static str)> {
    Some((file.path().to_str()?, file.contents_utf8()?))
}

/// Every text file under the pstack and team-kit roots of a vendored tree, by its path in that
/// tree. The manifests beside the roots and the retired files are left out.
fn vendored(tree: &'static Dir<'static>) -> Vec<(&'static str, &'static str)> {
    [PSTACK_ROOT, TEAM_KIT_ROOT]
        .into_iter()
        .filter_map(|root| tree.get_dir(root))
        .flat_map(files)
        .filter_map(text_file)
        .filter(|(path, _)| !RETIRED.contains(path))
        .collect()
}

/// A vendored file's page: pstack's by its path in the plugin, a team-kit skill beside pstack's
/// under `skills/`, and any other team-kit file under its own root.
fn page_key(path: &str) -> String {
    if let Some(relative) = path.strip_prefix(PSTACK_ROOT) {
        return relative.trim_start_matches(PATH_SEPARATOR).to_owned();
    }
    let relative = path
        .strip_prefix(TEAM_KIT_ROOT)
        .unwrap_or(path)
        .trim_start_matches(PATH_SEPARATOR);
    match relative.starts_with(SKILLS_DIR) {
        true => relative.to_owned(),
        false => path.to_owned(),
    }
}

/// `skills/<name>/SKILL.md`'s name, under either root.
fn skill_of(path: &str) -> Option<&str> {
    let path = path.strip_prefix(TEAM_KIT_ROOT).unwrap_or(path);
    let rest = path
        .trim_start_matches(PATH_SEPARATOR)
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

fn frontmatter(text: &str) -> Frontmatter {
    split_frontmatter(text)
        .0
        .and_then(|yaml| serde_yaml::from_str(yaml).ok())
        .unwrap_or_default()
}

fn body(text: &str) -> &str {
    split_frontmatter(text).1
}

/// Resolves `.` and `..` and drops empty segments; `None` when the path climbs out of the plugin.
fn normalize(path: &str) -> Option<String> {
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

fn headed(path: &str, text: &str) -> String {
    let dir = path.rsplit_once(PATH_SEPARATOR).map_or("", |(dir, _)| dir);
    format!(
        "Page `{NAME}/{path}`. A relative path in it resolves against `{NAME}/{dir}/`.\n\n{text}"
    )
}

fn entry(skill: &Skill) -> String {
    format!("- `{NAME}/{}`: {}\n", skill.name, skill.summary())
}

fn first_sentence(text: &str) -> &str {
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
    use std::collections::BTreeSet;

    use regex::Regex;
    use sha2::{Digest, Sha256};
    use test_case::test_case;

    use super::*;
    use crate::roles::{ModelFacts, StoredRoles};

    const POTETO_MODE: &str = "poteto-mode";
    const SEARCH_ENTRY: &str = "- pstack/";
    const BUGBOT_CORRECTION: &str =
        "No word in pstack starts with \"bugbto\", so it also matched \"bugbot\".";
    const MISSING_SKILL_MSG: &str = "the index must list every skill that is not a principle";
    const DANGLING_MSG: &str = "a relative link must resolve to a page";
    const NAME_MISMATCH_MSG: &str = "a skill's frontmatter name must be its directory";
    const DRIFT_MSG: &str = "upstream.json must hash every vendored file; run `make sync-pstack`";
    const UNTRANSLATED_MSG: &str =
        "these lines still name Cursor; translate them with the pstack-translate skill";
    const BARE_COMMAND_MSG: &str = "a command the user types is `/pstack:<name>`";
    const ORPHAN_MSG: &str = "a translation's upstream file is gone; delete or move it";
    const STALE_MSG: &str = "upstream changed since this translation; retranslate it, then \
         `scripts/sync-pstack.py --mark` it";
    const TRANSLATIONS: &str = "translations.json";
    const UNLISTED_MSG: &str = "translations.json must list exactly the translated files";
    /// Links upstream writes as templates for the model to fill in.
    const PLACEHOLDER_LINKS: &[&str] = &["url"];

    const ROLES_HEADING: &str = "## Model roles";
    const POINTING_PAGE: &str = "skills/point/SKILL.md";
    const POINTING_TEXT: &str = "---\nname: point\ndescription: Points.\n---\n\nUse the Panel \
         rows of the **Model roles** table below for zebra reviews.\n";
    const UNSERVED_MSG: &str = "a retired file must not be served, listed, or searched";
    const VANISHED_MSG: &str = "a retired file is gone upstream; drop it from RETIRED";
    const UNPOINTED_MSG: &str =
        "a page that names a kind's rows must point at the Model roles table";
    const KIND_ROWS: &str = r"\b(Code|Hardest|Judgment|Verify|Panel|Cross-judge pool) rows?\b";

    fn roles() -> RoleTable {
        RoleTable::resolve(
            &StoredRoles::default(),
            &ModelFacts {
                favorites: Vec::new(),
                flagships: Vec::new(),
                task_model_choice: true,
                known: &|_| None,
            },
        )
    }

    fn pointing_library() -> Library {
        let upstream = library().upstream().clone();
        Library::new(
            BTreeMap::from([(POINTING_PAGE.to_owned(), POINTING_TEXT)]),
            upstream,
        )
    }

    #[test]
    fn a_page_that_points_at_the_roles_ends_with_the_table() {
        let roles = roles();
        let page = pointing_library().load("/point", &roles).unwrap();
        assert!(page.ends_with(&roles.render()), "{page}");
        let found = pointing_library().load("?zebra", &roles).unwrap();
        assert!(found.contains(POINTING_PAGE), "{found}");
        assert!(!found.contains(ROLES_HEADING), "{found}");
    }

    #[test]
    fn a_page_that_does_not_point_at_the_roles_has_no_table() {
        let page = library().load("/skills/how/SKILL.md", &roles()).unwrap();
        assert_eq!(
            page.contains(ROLES_HEADING),
            library().pages["skills/how/SKILL.md"].contains(MODEL_ROLES_REF)
        );
    }

    #[test]
    fn the_index_carries_the_roles_table() {
        let roles = roles();
        assert!(library().index(&roles).contains(&roles.render()));
    }

    #[test]
    fn a_retired_file_is_neither_served_nor_a_skill() {
        for retired in RETIRED {
            assert!(
                UPSTREAM.get_file(retired).is_some(),
                "{VANISHED_MSG}: {retired}"
            );
            let key = page_key(retired);
            assert!(!library().pages.contains_key(&key), "{UNSERVED_MSG}: {key}");
            if let Some(name) = skill_of(&key) {
                assert!(library().skill(name).is_none(), "{UNSERVED_MSG}: {name}");
                let found = library().load(&format!("?{name}"), &roles()).unwrap();
                assert!(!found.contains(&key), "{UNSERVED_MSG}: {found}");
            }
        }
    }

    #[test]
    fn a_page_naming_a_kinds_rows_points_at_the_roles_table() {
        let kind_rows = Regex::new(KIND_ROWS).unwrap();
        let unpointed: Vec<&String> = library()
            .pages
            .iter()
            .filter(|(_, text)| kind_rows.is_match(text) && !text.contains(MODEL_ROLES_REF))
            .map(|(page, _)| page)
            .collect();
        assert!(unpointed.is_empty(), "{UNPOINTED_MSG}: {unpointed:?}");
    }

    #[test_case("/poteto-mode", "skills/poteto-mode/SKILL.md" ; "skill_shorthand")]
    #[test_case("/skills/how/SKILL.md", "skills/how/SKILL.md" ; "full_path")]
    #[test_case("/poteto-mode/playbooks/bug-fix.md", "skills/poteto-mode/playbooks/bug-fix.md" ; "skill_relative_file")]
    #[test_case("/skills/poteto-mode/../how/SKILL.md", "skills/how/SKILL.md" ; "parent_segment")]
    #[test_case("/docs/guide", "docs/guide/README.md" ; "directory_readme")]
    #[test_case("/deslop", "skills/deslop/SKILL.md" ; "team_kit_skill")]
    #[test_case("/agents/poteto-agent.md", "agents/poteto-agent.md" ; "agent")]
    #[test_case("/caudra/automations/pstack-watch-pr.rhai", "caudra/automations/pstack-watch-pr.rhai" ; "caudra_automation")]
    fn an_address_loads_its_page(address: &str, path: &str) {
        let page = library().load(address, &roles()).unwrap();
        assert!(page.starts_with(&format!("Page `{NAME}/{path}`")), "{page}");
    }

    #[test]
    fn a_translation_replaces_its_upstream_page() {
        for (path, text) in vendored(&TRANSLATED) {
            assert_eq!(library().pages[&page_key(path)], text, "{path}");
        }
    }

    #[test]
    fn some_page_says_how_to_arm_each_caudra_automation() {
        const UNNAMED_MSG: &str = "a translated page must say how to arm every Caudra automation";
        const ARM_COMMAND: &str = "/automations arm";
        let stems: Vec<&str> = automations().map(|(stem, _)| stem).collect();
        assert_eq!(
            stems.len(),
            library()
                .pages
                .keys()
                .filter(|path| path.starts_with(CAUDRA_AUTOMATIONS_DIR))
                .count()
        );
        for stem in stems {
            let arm = format!("{ARM_COMMAND} {stem}");
            let named = library().pages.iter().any(|(page, text)| {
                !page.starts_with(CAUDRA_AUTOMATIONS_DIR) && text.contains(&arm)
            });
            assert!(named, "{UNNAMED_MSG}: {arm}");
        }
    }

    #[test]
    fn an_address_cannot_climb_out_of_the_plugin() {
        assert!(library().load("/../../etc/passwd", &roles()).is_err());
    }

    #[test]
    fn an_unknown_address_names_the_nearest_pages() {
        let error = library().load("/poteto-mod", &roles()).unwrap_err();
        assert!(
            error.contains("`pstack/skills/poteto-mode/SKILL.md`"),
            "{error}"
        );
    }

    #[test]
    fn a_directory_without_an_index_lists_its_pages() {
        let listing = library().load("/poteto-mode/playbooks", &roles()).unwrap();
        assert!(listing.contains("`pstack/skills/poteto-mode/playbooks/feature.md`"));
    }

    #[test_case("bugbot triage", "skills/poteto-mode/references/bugbot-triage.md" ; "terms_in_the_path_heading")]
    #[test_case("lookback_hours", "caudra/automations/benny-reproduce.rhai" ; "script_without_headings")]
    fn a_search_lists_the_best_page_first(query: &str, path: &str) {
        let found = library().load(&format!("?{query}"), &roles()).unwrap();
        let first = found.lines().find(|line| line.starts_with(SEARCH_ENTRY));
        assert_eq!(
            first
                .and_then(|line| line.split_once(" ("))
                .map(|(address, _)| address),
            Some(format!("{SEARCH_ENTRY}{path}").as_str()),
            "{found}"
        );
    }

    #[test]
    fn a_search_lists_each_page_once() {
        let found = library().load("?skill", &roles()).unwrap();
        let addresses: Vec<&str> = found
            .lines()
            .filter(|line| line.starts_with(SEARCH_ENTRY))
            .collect();
        let distinct: BTreeSet<&str> = addresses.iter().copied().collect();
        assert_eq!(addresses.len(), SEARCH_LIMIT, "{found}");
        assert_eq!(distinct.len(), addresses.len(), "{found}");
    }

    #[test]
    fn a_misspelt_search_word_names_its_correction() {
        let found = library().load("?bugbto triage", &roles()).unwrap();
        assert!(found.starts_with(BUGBOT_CORRECTION), "{found}");
        assert!(
            found.contains("poteto-mode/references/bugbot-triage.md"),
            "{found}"
        );
    }

    #[test]
    fn the_index_lists_every_skill_and_principle() {
        let index = library().index(&roles());
        for skill in library().skills() {
            assert!(
                index.contains(&format!("`{NAME}/{}`", skill.name)),
                "{MISSING_SKILL_MSG}: {}",
                skill.name
            );
        }
        assert!(library().skill(POTETO_MODE).is_some());
    }

    #[test]
    fn every_skill_is_named_by_its_directory() {
        for (path, text) in &library().pages {
            if let Some(directory) = skill_of(path) {
                assert_eq!(
                    frontmatter(text).name.as_deref(),
                    Some(directory),
                    "{NAME_MISMATCH_MSG}"
                );
            }
        }
    }

    #[test]
    fn every_relative_link_resolves_to_a_page() {
        let link = Regex::new(r"\]\(([^)\s]+)\)").unwrap();
        for (path, text) in &library().pages {
            if !path.ends_with(MARKDOWN) {
                continue;
            }
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            for capture in link.captures_iter(text) {
                let href = capture[1].split(['#', '?']).next().unwrap_or_default();
                if href.is_empty() || href.contains(':') || PLACEHOLDER_LINKS.contains(&href) {
                    continue;
                }
                let target = normalize(&format!("{dir}/{href}")).unwrap();
                let found =
                    library().page(&target).is_some() || library().listing(&target).is_some();
                assert!(found, "{DANGLING_MSG}: {path} -> {href}");
            }
        }
    }

    #[test]
    fn the_manifest_hashes_every_vendored_file() {
        let manifest: serde_json::Value = serde_json::from_str(
            UPSTREAM
                .get_file(MANIFEST)
                .unwrap()
                .contents_utf8()
                .unwrap(),
        )
        .unwrap();
        let listed: BTreeSet<String> = manifest["files"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut vendored = BTreeSet::new();
        for file in files(&UPSTREAM) {
            let path = file.path().to_str().unwrap();
            if path == MANIFEST {
                continue;
            }
            let digest: String = Sha256::digest(file.contents())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(manifest["files"][path], digest, "{DRIFT_MSG}: {path}");
            vendored.insert(path.to_owned());
        }
        assert_eq!(vendored, listed, "{DRIFT_MSG}");
    }

    /// The upstream hash each translation was made from must still be upstream's, so a sync that
    /// changes a translated file fails here until the translation follows it.
    #[test]
    fn every_translation_follows_its_upstream_file() {
        let read = |dir: &Dir<'static>, file: &str| -> serde_json::Value {
            serde_json::from_str(dir.get_file(file).unwrap().contents_utf8().unwrap()).unwrap()
        };
        let upstream = read(&UPSTREAM, MANIFEST);
        let recorded = read(&TRANSLATED, TRANSLATIONS);
        let recorded = recorded["files"].as_object().unwrap();
        let translated: BTreeSet<&str> = vendored(&TRANSLATED)
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        for path in &translated {
            let base = &upstream["files"][*path];
            assert!(!base.is_null(), "{ORPHAN_MSG}: {path}");
            assert_eq!(recorded.get(*path), Some(base), "{STALE_MSG}: {path}");
        }
        let listed: BTreeSet<&str> = recorded.keys().map(String::as_str).collect();
        assert_eq!(listed, translated, "{UNLISTED_MSG}");
    }

    /// Every served page reads as Caudra's: a page that still names one of these was not
    /// translated. `.caudra/skills/pstack-translate` says what each becomes. A hard-coded `/tmp`
    /// counts: Caudra gives each project a scratch directory, and writes outside it prompt.
    const CURSOR_TERMS: &[&str] = &[
        r"`Task`|\bTask (tool|subagent|call)s?\b",
        r"AskQuestion",
        r"generalPurpose",
        r"subagent_type",
        r"readonly: ?(true|false)|\breadonly mode\b|\bAsk mode\b",
        r"run_in_background|is_background",
        r#"environment: ?"?(cloud|local)"#,
        r"cloud_base_branch",
        r"file_attachments",
        r"\b(claude|grok|gpt|gemini)-[a-z0-9.-]*-(low|medium|high|xhigh|max)(-fast)?\b",
        r"inherit-parent",
        r"\.mdc\b|alwaysApply|always-applied rule",
        r"\.cursor\b",
        r"agent-transcripts",
        r"[Cc]ursor [Aa]utomations?|cursor\.(sh|com)/automations|api2\.cursor|update_state|SendToUser",
        r"[Cc]loud [Aa]gents?",
        r"built-in PR tool",
        r"\bcreate-skill\b",
        r"check-plan\.mjs|worktree-audit\.sh|\blog\.sh\b|watch-pr/watch-pr|scripts/watch-pr|orch\.ts",
        r"\bbunx?\b|\bnode [^ ]*\.m?js",
        r"/loop\b",
        r"cursor\.com/docs",
        r"\bCursor\b",
        r#"(^|[\s`"'(=:])/tmp\b"#,
        r"\b(claude-(opus|sonnet|haiku)|grok|gemini)-[0-9]",
        r"\bgpt-[0-9]",
        r"setup-pstack",
        r"pstack/models\.md",
    ];
    /// Pages that name a Cursor term on purpose, and the term. The synthesizer's `gpt-4` is its
    /// example of a fact that goes stale, not a model to run.
    const CURSOR_TERMS_ALLOWED: &[(&str, &str)] = &[
        ("README.md", r"\bCursor\b"),
        ("skills/reflect/references/synthesizer.md", r"\bgpt-[0-9]"),
    ];
    const LICENSE_FILE: &str = "LICENSE";

    fn untranslated(term: &Regex) -> Vec<String> {
        library()
            .pages
            .iter()
            .filter(|(page, _)| {
                !page.starts_with(CAUDRA_AUTOMATIONS_DIR) && !page.ends_with(LICENSE_FILE)
            })
            .filter(|(page, _)| {
                !CURSOR_TERMS_ALLOWED
                    .iter()
                    .any(|(allowed, pattern)| allowed == page && *pattern == term.as_str())
            })
            .flat_map(|(page, text)| {
                text.lines()
                    .enumerate()
                    .filter(|(_, line)| term.is_match(line))
                    .map(move |(number, line)| format!("{page}:{}: {}", number + 1, line.trim()))
            })
            .collect()
    }

    #[test]
    fn no_page_names_a_cursor_term() {
        let found: Vec<String> = CURSOR_TERMS
            .iter()
            .flat_map(|pattern| untranslated(&Regex::new(pattern).unwrap()))
            .collect();
        assert!(
            found.is_empty(),
            "{UNTRANSLATED_MSG}:\n{}",
            found.join("\n")
        );
    }

    #[test]
    fn a_skill_command_carries_the_pstack_scope() {
        let names: Vec<String> = library()
            .skills()
            .iter()
            .map(|skill| regex::escape(&skill.name))
            .collect();
        let bare = Regex::new(&format!(
            r"(?m)(^|[\s`(\[*'])/({})($|[^\w-])",
            names.join("|")
        ))
        .unwrap();
        let found = untranslated(&bare);
        assert!(
            found.is_empty(),
            "{BARE_COMMAND_MSG}:\n{}",
            found.join("\n")
        );
    }
}
