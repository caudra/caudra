//! The vendored pstack as pages: every file by its path in the plugin, the shorthands the skills'
//! own routing uses, an index the `pstack` skill opens with, and a word search.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use include_dir::{Dir, File, include_dir};
use serde::Deserialize;

use crate::models::Roles;

static UPSTREAM: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/upstream");
static AUTOMATIONS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/automations");
static LIBRARY: LazyLock<Library> = LazyLock::new(Library::embedded);

/// The skill's name, and the prefix of every address.
pub const NAME: &str = "pstack";
pub const SKILL_DESCRIPTION: &str = "poteto's pstack: rigorous engineering skills (poteto-mode, \
     bug fixes, plans, reviews, orchestration). Load only when the user asks for pstack, poteto, \
     or one of its skills, or sends a `/pstack:<skill>` command; the index lists every skill.";
pub const ADAPTER: &str = include_str!("../adapter.md");
pub const ADAPTER_PAGE: &str = "adapter";
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
const SUGGESTION_LIMIT: usize = 5;
const SNIPPET_CHARS: usize = 160;
/// Entry points the index lists first, in this order.
const START_HERE: &[&str] = &["poteto-mode", "poteto-help", "setup-pstack"];
const GUIDE: &str = "docs/guide/README.md";
const BENNY: &str = "automations/benny/README.md";
/// Where Caudra's ports of pstack's automations sit among the pages.
const CAUDRA_AUTOMATIONS_DIR: &str = "caudra/automations";

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
}

pub fn library() -> &'static Library {
    &LIBRARY
}

impl Library {
    fn embedded() -> Self {
        let mut pages = BTreeMap::new();
        let roots = [PSTACK_ROOT, TEAM_KIT_ROOT].map(|root| UPSTREAM.get_dir(root));
        for (root, dir) in [PSTACK_ROOT, TEAM_KIT_ROOT].into_iter().zip(roots) {
            let Some(dir) = dir else { continue };
            for file in files(dir) {
                let Some((path, text)) = text_file(file) else {
                    continue;
                };
                let relative = path.strip_prefix(root).unwrap_or(path);
                let relative = relative.trim_start_matches(PATH_SEPARATOR);
                match root == TEAM_KIT_ROOT && !relative.starts_with(SKILLS_DIR) {
                    true => pages.insert(path.to_owned(), text),
                    false => pages.insert(relative.to_owned(), text),
                };
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
    pub fn index(&self, roles: &Roles) -> String {
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
             The skills are written for Cursor. Load `{NAME}/{ADAPTER_PAGE}` once per session \
             before you follow one: it maps their Cursor tools, subagent fields, models, paths, \
             and scripts to Caudra's. Run a skill only when the user asks for pstack or one of \
             its skills, or sends a `/{NAME}:<skill>` command.\n\n## Start here\n\n"
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
             - `{NAME}/{CAUDRA_AUTOMATIONS_DIR}`: benny and watch-pr's drive mode as Caudra \
             automations; `{NAME}/{ADAPTER_PAGE}` says how to install them.\n\n## Model roles\n\n{}\n",
            roles.describe()
        ));
        index
    }

    /// One page by the address that follows `pstack`: `/path`, or `?words` to search.
    pub fn load(&self, address: &str) -> Result<String, String> {
        if let Some(words) = address.strip_prefix(SEARCH_SEPARATOR) {
            return Ok(self.search(words));
        }
        let requested = address.trim_start_matches(PATH_SEPARATOR);
        if requested == ADAPTER_PAGE {
            return Ok(ADAPTER.to_owned());
        }
        let path = normalize(requested).ok_or_else(|| {
            format!("`{NAME}/{requested}` leaves pstack; addresses stay inside the plugin")
        })?;
        if let Some((found, text)) = self.page(&path) {
            return Ok(headed(&found, text));
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

    fn search(&self, words: &str) -> String {
        let terms: Vec<String> = words.split_whitespace().map(str::to_lowercase).collect();
        if terms.is_empty() {
            return format!("Search with `{NAME}?<words>`, such as `{NAME}?flaky test`.");
        }
        let hits: Vec<String> = self
            .pages
            .iter()
            .filter(|(_, text)| {
                let lower = text.to_lowercase();
                terms.iter().all(|term| lower.contains(term.as_str()))
            })
            .take(SEARCH_LIMIT)
            .map(|(path, text)| match snippet(text, &terms[0]) {
                Some(line) => format!("- `{NAME}/{path}`: {line}"),
                None => format!("- `{NAME}/{path}`"),
            })
            .collect();
        match hits.is_empty() {
            true => format!("No pstack page contains every word of `{words}`."),
            false => format!(
                "Pages containing every word of `{words}`:\n\n{}",
                hits.join("\n")
            ),
        }
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
        "Page `{NAME}/{path}`. A relative path in it resolves against `{NAME}/{dir}/`. Map its \
         Cursor names with `{NAME}/{ADAPTER_PAGE}`.\n\n{text}"
    )
}

fn entry(skill: &Skill) -> String {
    format!("- `{NAME}/{}`: {}\n", skill.name, skill.summary())
}

fn first_sentence(text: &str) -> &str {
    text.find(". ").map_or(text, |end| &text[..=end])
}

fn snippet(text: &str, term: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|line| line.to_lowercase().contains(term))?
        .trim();
    Some(line.chars().take(SNIPPET_CHARS).collect())
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

    const POTETO_MODE: &str = "poteto-mode";
    const MISSING_SKILL_MSG: &str = "the index must list every skill that is not a principle";
    const DANGLING_MSG: &str = "a relative link must resolve to a page";
    const NAME_MISMATCH_MSG: &str = "a skill's frontmatter name must be its directory";
    const DRIFT_MSG: &str = "upstream.json must hash every vendored file; run `make sync-pstack`";
    const UNMAPPED_MSG: &str = "the adapter needs a row for a Cursor name the skills use";
    /// Links upstream writes as templates for the model to fill in.
    const PLACEHOLDER_LINKS: &[&str] = &["url"];

    fn roles() -> Roles {
        Roles::Missing {
            path: "/config/pstack/models.md".to_owned(),
        }
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
        let page = library().load(address).unwrap();
        assert!(page.starts_with(&format!("Page `{NAME}/{path}`")), "{page}");
    }

    #[test]
    fn the_adapter_page_is_served_whole() {
        assert_eq!(library().load("/adapter").unwrap(), ADAPTER);
    }

    #[test]
    fn the_adapter_names_every_caudra_automation() {
        const UNNAMED_MSG: &str = "the adapter must say how to install every Caudra automation";
        let automations: Vec<&String> = library()
            .pages
            .keys()
            .filter(|path| path.starts_with(CAUDRA_AUTOMATIONS_DIR))
            .collect();
        assert!(!automations.is_empty());
        for path in automations {
            assert!(ADAPTER.contains(&format!("{NAME}/{path}")), "{UNNAMED_MSG}: {path}");
        }
    }

    #[test]
    fn an_address_cannot_climb_out_of_the_plugin() {
        assert!(library().load("/../../etc/passwd").is_err());
    }

    #[test]
    fn an_unknown_address_names_the_nearest_pages() {
        let error = library().load("/poteto-mod").unwrap_err();
        assert!(error.contains("`pstack/skills/poteto-mode/SKILL.md`"), "{error}");
    }

    #[test]
    fn a_directory_without_an_index_lists_its_pages() {
        let listing = library().load("/poteto-mode/playbooks").unwrap();
        assert!(listing.contains("`pstack/skills/poteto-mode/playbooks/feature.md`"));
    }

    #[test]
    fn a_search_finds_pages_with_every_word() {
        let found = library().load("?bugbot triage").unwrap();
        assert!(found.contains("poteto-mode/references/bugbot-triage.md"), "{found}");
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
                let found = library().page(&target).is_some() || library().listing(&target).is_some();
                assert!(found, "{DANGLING_MSG}: {path} -> {href}");
            }
        }
    }

    #[test]
    fn the_manifest_hashes_every_vendored_file() {
        let manifest: serde_json::Value = serde_json::from_str(
            UPSTREAM.get_file(MANIFEST).unwrap().contents_utf8().unwrap(),
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

    /// The Cursor names upstream's Claude Code generator checks its adapter for, plus the ones
    /// only Caudra maps. A skill that starts using one needs its row in `adapter.md`.
    #[test_case(r"`Task`|\bTask (tool|subagent|call)", "`Task`")]
    #[test_case("AskQuestion", "`AskQuestion`")]
    #[test_case(r"\b[Tt]odo ?[Ll]ist\b", "todolist")]
    #[test_case("generalPurpose", "generalPurpose")]
    #[test_case("poteto-agent", "poteto-agent")]
    #[test_case("comment-sicko", "comment-sicko")]
    #[test_case(r"readonly: (true|false)", "`readonly: true`")]
    #[test_case(r#"environment: "(cloud|local)""#, r#"`environment: "cloud"`"#)]
    #[test_case("cloud_base_branch", "cloud_base_branch")]
    #[test_case("run_in_background", "`run_in_background: true`")]
    #[test_case("`resume`", "`resume`")]
    #[test_case("file_attachments", "`file_attachments`")]
    #[test_case(r"\bclaude-[a-z]+-\d", "claude-")]
    #[test_case(r"\bgrok-\d", "grok-")]
    #[test_case(r"\bgpt-\d", "gpt-")]
    #[test_case("inherit-parent", "inherit-parent")]
    #[test_case(r"pstack-models\.mdc", "pstack-models.mdc")]
    #[test_case(r"\.cursor/skills", ".cursor/skills/")]
    #[test_case(r"\.cursor/plugins|\.cursor-plugin", "~/.cursor/plugins/")]
    #[test_case(r"\.cursor/rules|always-applied rule", ".cursor/rules/")]
    #[test_case(r"agent-transcripts|\.cursor/projects", "agent-transcripts")]
    #[test_case(r"Bugbot", "Bugbot")]
    #[test_case(r"Cursor [Aa]utomation|cursor\.(sh|com)/automations", "Cursor Automations")]
    #[test_case(r"[Cc]loud [Aa]gent", "Cursor cloud agents")]
    #[test_case(r"built-in PR tool", "A built-in PR tool")]
    #[test_case(r"create-skill", "`create-skill`")]
    #[test_case(r"check-plan\.mjs", "check-plan.mjs")]
    #[test_case(r"watch-pr", "watch-pr")]
    #[test_case(r"worktree-audit\.sh", "worktree-audit.sh")]
    #[test_case(r"log\.sh", "log.sh")]
    #[test_case(r"`orch ", "`orch <command>")]
    #[test_case(r"/loop\b", "/loop")]
    fn the_adapter_maps_every_cursor_name(pattern: &str, row: &str) {
        let term = Regex::new(pattern).unwrap();
        let used = library().pages.values().any(|text| term.is_match(text));
        assert!(!used || ADAPTER.contains(row), "{UNMAPPED_MSG}: {row}");
    }
}
