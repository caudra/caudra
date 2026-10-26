//! Matt Pocock's skills, served as pages of one `mattpocock` skill.
//!
//! `upstream/` is the copy `make sync-mattpocock` writes, each skill flattened from
//! `skills/<bucket>/<name>` to `skills/<name>`. `translated/` replaces each file that names Claude
//! Code's tools, paths, or features with its Caudra translation, and `caudra/` holds the pages
//! Caudra adds, such as the issue tracker the publishing skills share
//! (`.caudra/skills/skillpack-translate`).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::LazyLock;

use caudra_skillpack::{Library, MANIFEST, Skill, TRANSLATIONS, Upstream, files, text_file};
use include_dir::{Dir, include_dir};

static UPSTREAM: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/upstream");
static TRANSLATED: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/translated");
static CAUDRA: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/caudra");
static LIBRARY: LazyLock<Library> = LazyLock::new(embedded);

/// The skill's name, and the prefix of every address.
pub const NAME: &str = "mattpocock";
pub const SKILL_DESCRIPTION: &str = "Matt Pocock's engineering skills. Load when the user wants \
     to grill or stress-test a plan, build test-first (TDD), diagnose a hard bug or performance \
     regression, review changes against standards and spec, write a PR body, design deep modules \
     and seams, sharpen domain terms (GLOSSARY.md, ADRs), or write docs for agents; or sends a \
     `/mattpocock:<skill>` command. The index lists every skill.";
/// Where the pages Caudra adds sit among the vendored ones.
const CAUDRA_DIR: &str = "caudra";
const ISSUE_TRACKER: &str = "caudra/issue-tracker.md";
const LICENSE: &str = "LICENSE";

pub fn library() -> &'static Library {
    &LIBRARY
}

fn embedded() -> Library {
    let mut pages = BTreeMap::new();
    for (tree, skipped) in [(&UPSTREAM, MANIFEST), (&TRANSLATED, TRANSLATIONS)] {
        for (path, text) in files(tree).into_iter().filter_map(text_file) {
            if path != skipped {
                pages.insert(path.to_owned(), text);
            }
        }
    }
    for (path, text) in files(&CAUDRA).into_iter().filter_map(text_file) {
        pages.insert(format!("{CAUDRA_DIR}/{path}"), text);
    }
    Library::new(NAME, pages, Upstream::read(&UPSTREAM))
}

/// What loading `mattpocock` itself returns: how to read the rest, and every skill, split by
/// who starts it.
pub fn index() -> String {
    let library = library();
    let upstream = library.upstream();
    let (user_only, model): (Vec<&Skill>, Vec<&Skill>) =
        library.skills().iter().partition(|skill| skill.user_only);
    let entries = |skills: Vec<&Skill>| -> String {
        skills
            .into_iter()
            .map(|skill| library.entry(skill))
            .collect()
    };
    format!(
        "# mattpocock {version}\n\n\
         Matt Pocock's skills for real engineering, bundled from mattpocock/skills at `{short}` \
         under the MIT license (`{NAME}/{LICENSE}`). Every file is a page of this skill: load \
         `{NAME}/<skill>` for a skill, or `{NAME}/<path>` for any file by its path. \
         `{NAME}?<words>` searches every page.\n\n\
         ## Skills to reach for\n\n\
         Load one when its trigger fits the task, or when the user names it.\n\n{model}\n\
         ## Skills the user runs\n\n\
         Run one only when the user sends `/{NAME}:<skill>` or asks for it by name.\n\n{user_only}\n\
         ## Shared pages\n\n\
         - `{NAME}/{ISSUE_TRACKER}`: where `to-spec` and `to-tickets` publish and `code-review` \
         finds a spec, and what each does in Plan mode.\n",
        version = upstream.version,
        short = upstream.short_commit(),
        model = entries(model),
        user_only = entries(user_only),
    )
}

#[cfg(test)]
mod tests {
    use caudra_skillpack::checks;
    use regex::Regex;

    use super::*;

    const UNSHIPPED_MSG: &str = "a page loads a skill the pack does not ship";
    const MISSING_SKILL_MSG: &str = "the index must list every skill";
    const MISPLACED_MSG: &str = "a skill the user runs must be listed under that heading";
    const USER_HEADING: &str = "## Skills the user runs";
    const GRILLING: &str = "grilling";
    const TO_SPEC: &str = "to-spec";
    /// The glossary format's example map links to files in the user's project, not to pages.
    const PLACEHOLDER_LINKS: &[&str] = &[
        "./src/ordering/GLOSSARY.md",
        "./src/billing/GLOSSARY.md",
        "./src/fulfillment/GLOSSARY.md",
    ];
    /// Every served page reads as Caudra's: a page that still names one of these was not
    /// translated. `.caudra/skills/skillpack-translate/references/mattpocock.md` says what each
    /// becomes.
    const CLAUDE_TERMS: &[&str] = &[
        r"Skill tool",
        r"[Ss]ub-?agents?\b",
        r"[Bb]ackground agent",
        r"AskUserQuestion|TodoWrite",
        r"\bclaude --|\.claude/|Claude Code",
        r"skills\.sh|npx skills",
        r"setup-matt-pocock-skills",
        r"%TEMP%|\bOS temp|temporary directory of the user",
        r#"(^|[\s`"'(=:])/tmp\b"#,
        r"`Task`|\bTask (tool|subagent|call)s?\b",
    ];

    fn unread(page: &str) -> bool {
        page == LICENSE
    }

    #[test]
    fn every_skill_is_named_by_its_directory() {
        checks::every_skill_is_named_by_its_directory(library());
    }

    #[test]
    fn every_relative_link_resolves_to_a_page() {
        checks::every_relative_link_resolves(library(), PLACEHOLDER_LINKS);
    }

    #[test]
    fn the_manifest_hashes_every_vendored_file() {
        checks::the_manifest_hashes_every_vendored_file(library(), &UPSTREAM);
    }

    #[test]
    fn every_translation_follows_its_upstream_file() {
        checks::every_translation_follows_its_upstream_file(library(), &UPSTREAM, &TRANSLATED);
    }

    #[test]
    fn no_page_names_a_claude_code_term() {
        checks::no_page_names_a_foreign_term(library(), CLAUDE_TERMS, &[], unread);
    }

    #[test]
    fn a_skill_command_carries_the_mattpocock_scope() {
        checks::every_skill_command_carries_the_scope(library(), unread);
    }

    #[test]
    fn a_translation_replaces_its_upstream_page() {
        for (path, text) in files(&TRANSLATED).into_iter().filter_map(text_file) {
            if path != TRANSLATIONS {
                assert_eq!(library().pages()[path], text, "{path}");
            }
        }
    }

    #[test]
    fn every_skill_a_page_loads_is_shipped() {
        let loaded = Regex::new(&format!(r"`{NAME}/([a-z-]+)`")).unwrap();
        for (path, text) in library().pages() {
            for capture in loaded.captures_iter(text) {
                let name = &capture[1];
                assert!(
                    library().page(name).is_some() || library().listing(name).is_some(),
                    "{UNSHIPPED_MSG}: {path} -> {name}"
                );
            }
        }
    }

    #[test]
    fn the_index_lists_each_skill_under_who_starts_it() {
        let index = index();
        let (model, user) = index.split_once(USER_HEADING).unwrap();
        for skill in library().skills() {
            let entry = format!("`{NAME}/{}`", skill.name);
            assert!(
                index.contains(&entry),
                "{MISSING_SKILL_MSG}: {}",
                skill.name
            );
            let listed = match skill.user_only {
                true => user,
                false => model,
            };
            assert!(listed.contains(&entry), "{MISPLACED_MSG}: {}", skill.name);
        }
        assert!(!library().skill(GRILLING).unwrap().user_only);
        assert!(library().skill(TO_SPEC).unwrap().user_only);
    }

    #[test]
    fn the_issue_tracker_page_is_served() {
        let loaded = library().load(&format!("/{ISSUE_TRACKER}")).unwrap();
        assert!(loaded.page.is_some());
    }
}
