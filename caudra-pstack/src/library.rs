//! The vendored pstack as pages: every file by its path in the plugin, the shorthands the skills'
//! own routing uses, an index the `pstack` skill opens with, and a word search.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use caudra_skillpack::{
    MARKDOWN, SKILLS_DIR, Skill, Upstream, body, files, first_sentence, frontmatter, text_file,
};
use include_dir::{Dir, include_dir};

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
const AGENTS_DIR: &str = "agents";
const PATH_SEPARATOR: char = '/';
const PRINCIPLE_PREFIX: &str = "principle-";
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

/// A `principle-*` leaf, read from within other skills rather than run on its own.
pub fn is_principle(skill: &Skill) -> bool {
    skill.name.starts_with(PRINCIPLE_PREFIX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub body: String,
}

pub struct Library {
    /// Every page by its path in the plugin, the team-kit skills under `skills/` beside pstack's.
    pages: caudra_skillpack::Library,
    agents: Vec<Agent>,
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
        Self::new(pages, Upstream::read(&UPSTREAM))
    }

    fn new(pages: BTreeMap<String, &'static str>, upstream: Upstream) -> Self {
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
            pages: caudra_skillpack::Library::new(NAME, pages, upstream),
            agents,
        }
    }

    pub fn pages(&self) -> &caudra_skillpack::Library {
        &self.pages
    }

    pub fn upstream(&self) -> &Upstream {
        self.pages.upstream()
    }

    pub fn skills(&self) -> &[Skill] {
        self.pages.skills()
    }

    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// What loading `pstack` itself returns: how to read the rest, the entry points, every
    /// skill, and the user's model roles.
    pub fn index(&self, roles: &RoleTable) -> String {
        let version = &self.upstream().version;
        let short = self.upstream().short_commit();
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
            if let Some(skill) = self.pages.skill(name) {
                index.push_str(&self.pages.entry(skill));
            }
        }
        index.push_str(&format!(
            "- `{NAME}/{GUIDE}`: the pstack guide, a first real task from setup to overnight runs.\n\n## Skills\n\n"
        ));
        let listed = self
            .skills()
            .iter()
            .filter(|skill| !is_principle(skill) && !START_HERE.contains(&skill.name.as_str()));
        for skill in listed {
            index.push_str(&self.pages.entry(skill));
        }
        let principles: Vec<String> = self
            .skills()
            .iter()
            .filter(|skill| is_principle(skill))
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
        let loaded = self.pages.load(address)?;
        let mut text = loaded.text;
        if loaded
            .page
            .is_some_and(|page| page.contains(MODEL_ROLES_REF))
        {
            text.push_str("\n\n");
            text.push_str(&roles.render());
        }
        Ok(text)
    }
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

#[cfg(test)]
mod tests {
    use caudra_skillpack::{SEARCH_LIMIT, checks, skill_of};
    use regex::Regex;
    use test_case::test_case;

    use super::*;
    use crate::roles::{ModelFacts, StoredRoles};

    const POTETO_MODE: &str = "poteto-mode";
    const SEARCH_ENTRY: &str = "- pstack/";
    const BUGBOT_CORRECTION: &str =
        "No word in pstack starts with \"bugbto\", so it also matched \"bugbot\".";
    const MISSING_SKILL_MSG: &str = "the index must list every skill that is not a principle";
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

    fn pages() -> &'static BTreeMap<String, &'static str> {
        library().pages().pages()
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
            pages()["skills/how/SKILL.md"].contains(MODEL_ROLES_REF)
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
            assert!(!pages().contains_key(&key), "{UNSERVED_MSG}: {key}");
            if let Some(name) = skill_of(&key) {
                assert!(
                    library().pages().skill(name).is_none(),
                    "{UNSERVED_MSG}: {name}"
                );
                let found = library().load(&format!("?{name}"), &roles()).unwrap();
                assert!(!found.contains(&key), "{UNSERVED_MSG}: {found}");
            }
        }
    }

    #[test]
    fn a_page_naming_a_kinds_rows_points_at_the_roles_table() {
        let kind_rows = Regex::new(KIND_ROWS).unwrap();
        let unpointed: Vec<&String> = pages()
            .iter()
            .filter(|(_, text)| kind_rows.is_match(text) && !text.contains(MODEL_ROLES_REF))
            .map(|(page, _)| page)
            .collect();
        assert!(unpointed.is_empty(), "{UNPOINTED_MSG}: {unpointed:?}");
    }

    #[test_case("/poteto-mode", "skills/poteto-mode/SKILL.md" ; "skill_shorthand")]
    #[test_case("/poteto-mode/playbooks/bug-fix.md", "skills/poteto-mode/playbooks/bug-fix.md" ; "skill_relative_file")]
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
            assert_eq!(pages()[&page_key(path)], text, "{path}");
        }
    }

    #[test]
    fn some_page_says_how_to_arm_each_caudra_automation() {
        const UNNAMED_MSG: &str = "a translated page must say how to arm every Caudra automation";
        const ARM_COMMAND: &str = "/automations arm";
        let stems: Vec<&str> = automations().map(|(stem, _)| stem).collect();
        assert_eq!(
            stems.len(),
            pages()
                .keys()
                .filter(|path| path.starts_with(CAUDRA_AUTOMATIONS_DIR))
                .count()
        );
        for stem in stems {
            let arm = format!("{ARM_COMMAND} {stem}");
            let named = pages().iter().any(|(page, text)| {
                !page.starts_with(CAUDRA_AUTOMATIONS_DIR) && text.contains(&arm)
            });
            assert!(named, "{UNNAMED_MSG}: {arm}");
        }
    }

    #[test]
    fn an_unknown_address_names_the_nearest_pages() {
        let error = library().load("/poteto-mod", &roles()).unwrap_err();
        assert!(
            error.contains("`pstack/skills/poteto-mode/SKILL.md`"),
            "{error}"
        );
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
        let distinct: std::collections::BTreeSet<&str> = addresses.iter().copied().collect();
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
        assert!(library().pages().skill(POTETO_MODE).is_some());
    }

    #[test]
    fn every_skill_is_named_by_its_directory() {
        checks::every_skill_is_named_by_its_directory(library().pages());
    }

    #[test]
    fn every_relative_link_resolves_to_a_page() {
        checks::every_relative_link_resolves(library().pages(), PLACEHOLDER_LINKS);
    }

    #[test]
    fn the_manifest_hashes_every_vendored_file() {
        checks::the_manifest_hashes_every_vendored_file(library().pages(), &UPSTREAM);
    }

    #[test]
    fn every_translation_follows_its_upstream_file() {
        checks::every_translation_follows_its_upstream_file(
            library().pages(),
            &UPSTREAM,
            &TRANSLATED,
        );
    }

    /// Every served page reads as Caudra's: a page that still names one of these was not
    /// translated. `.caudra/skills/skillpack-translate` says what each becomes. A hard-coded
    /// `/tmp` counts: Caudra gives each project a scratch directory, and writes outside it prompt.
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
        r"(statechart|tlc-to-chart|chart-to-tla|compare-charts)\.mjs|\.map\.mjs|tla2tools|--png\b",
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

    fn unread(page: &str) -> bool {
        page.starts_with(CAUDRA_AUTOMATIONS_DIR) || page.ends_with(LICENSE_FILE)
    }

    #[test]
    fn no_page_names_a_cursor_term() {
        checks::no_page_names_a_foreign_term(
            library().pages(),
            CURSOR_TERMS,
            CURSOR_TERMS_ALLOWED,
            unread,
        );
    }

    #[test]
    fn a_skill_command_carries_the_pstack_scope() {
        checks::every_skill_command_carries_the_scope(library().pages(), unread);
    }
}
