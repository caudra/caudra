//! Emil Kowalski's design-engineering skills, served as pages of one `emil` skill.
//!
//! `upstream/` is the copy `make sync-emil` writes. `translated/` replaces each file that names
//! another harness's tools, paths, or features with its Caudra translation, and `caudra/` holds
//! the pages Caudra adds, such as the visual check the skills share
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
pub const NAME: &str = "emil";
pub const SKILL_DESCRIPTION: &str = "Emil Kowalski's design-engineering skills. Load when the \
     user wants to build or polish an animation or transition, review motion code, find where \
     motion would help, fix janky animation, remove animations, name an effect, make a web app \
     feel native on a phone, apply Apple-style fluid interfaces, use Sonner toasts, write modern \
     Swift, stress-test a UI with worst-case data, or prototype UI variants; or sends a \
     `/emil:<skill>` command. The index lists every skill.";
/// Where the pages Caudra adds sit among the vendored ones.
const CAUDRA_DIR: &str = "caudra";
const VISUAL_CHECK: &str = "caudra/visual-check.md";
const UI_CHEATSHEET: &str = "ui-cheatsheet.md";
const PERFORMANCE_CHEATSHEET: &str = "performance-cheatsheet.md";
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

/// What loading `emil` itself returns: how to read the rest, and every skill, split by who starts
/// it.
pub fn index() -> String {
    let library = library();
    let (user_only, model): (Vec<&Skill>, Vec<&Skill>) =
        library.skills().iter().partition(|skill| skill.user_only);
    let entries = |skills: Vec<&Skill>| -> String {
        skills
            .into_iter()
            .map(|skill| library.entry(skill))
            .collect()
    };
    format!(
        "# {NAME}\n\n\
         Emil Kowalski's design-engineering skills, bundled from emilkowalski/skills at `{short}` \
         under the MIT license (`{NAME}/{LICENSE}`). Every file is a page of this skill: load \
         `{NAME}/<skill>` for a skill, or `{NAME}/<path>` for any file by its path. \
         `{NAME}?<words>` searches every page.\n\n\
         ## Skills to reach for\n\n\
         Load one when its trigger fits the task, or when the user names it.\n\n{model}\n\
         ## Skills the user runs\n\n\
         Run one only when the user sends `/{NAME}:<skill>` or asks for it by name.\n\n{user_only}\n\
         ## Shared pages\n\n\
         - `{NAME}/{VISUAL_CHECK}`: how the skills look at rendered UI, with screenshots from \
         tooling the project already has and `view_image`, and what a still image cannot show.\n\
         - `{NAME}/{UI_CHEATSHEET}` and `{NAME}/{PERFORMANCE_CHEATSHEET}`: one-screen summaries \
         of the UI and animation-performance rules.\n",
        short = library.upstream().short_commit(),
        model = entries(model),
        user_only = entries(user_only),
    )
}

#[cfg(test)]
mod tests {
    use caudra_skillpack::{SKILLS_DIR, checks};
    use regex::Regex;

    use super::*;

    const UNSHIPPED_MSG: &str = "a page loads a skill the pack does not ship";
    const MISSING_SKILL_MSG: &str = "the index must list every skill";
    const MISPLACED_MSG: &str = "a skill the user runs must be listed under that heading";
    const UNSCOPED_MSG: &str = "a sibling skill reads `emil/<name>`, or `/emil:<name>` when the \
         user runs it";
    const LOADS_USER_ONLY_MSG: &str = "a page the model may read suggests a user-only skill's \
         `/emil:<name>` command instead of loading it";
    const USER_HEADING: &str = "## Skills the user runs";
    const ANIMATE: &str = "animate";
    const USER_ONLY: &[&str] = &[
        "prototype",
        "review-animations",
        "pick-ui-library",
        "improve-animations",
        "break-ui",
    ];
    const TASK_TERM: &str = r"`Task`|\bTask (tool|subagent|call)s?\b";
    /// `write-swift` teaches Swift's own `Task`, not a harness's tool.
    const NAMED_ON_PURPOSE: &[(&str, &str)] = &[("skills/write-swift/SKILL.md", TASK_TERM)];
    /// Every served page reads as Caudra's: a page that still names one of these was not
    /// translated. `.caudra/skills/skillpack-translate/references/emil.md` says what each
    /// becomes.
    const FOREIGN_TERMS: &[&str] = &[
        r"Skill tool",
        r"[Ss]ub-?agents?\b",
        r"[Bb]ackground agent",
        r"AskUserQuestion|TodoWrite",
        r"\bclaude --|\.claude/|Claude Code",
        r"skills\.sh|npx skills",
        r"isolated worktree",
        r"browser tooling",
        r"non-interactively",
        r"I'm ready to",
        r#"(^|[\s`"'(=:])/tmp\b"#,
        TASK_TERM,
    ];

    fn unread(page: &str) -> bool {
        page == LICENSE
    }

    fn alternation<'a>(names: impl Iterator<Item = &'a str>) -> String {
        names.map(regex::escape).collect::<Vec<_>>().join("|")
    }

    #[test]
    fn every_skill_is_named_by_its_directory() {
        checks::every_skill_is_named_by_its_directory(library());
    }

    #[test]
    fn every_relative_link_resolves_to_a_page() {
        checks::every_relative_link_resolves(library(), &[]);
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
    fn no_page_names_a_foreign_term() {
        checks::no_page_names_a_foreign_term(library(), FOREIGN_TERMS, NAMED_ON_PURPOSE, unread);
    }

    #[test]
    fn a_skill_command_carries_the_emil_scope() {
        checks::every_skill_command_carries_the_scope(library(), unread);
    }

    /// A bare name in backticks, alone or with the arguments a user types after it. A name
    /// followed by `prop` or `props` is a component prop, such as Motion's `animate`.
    #[test]
    fn no_page_names_a_sibling_without_its_scope() {
        let names = alternation(library().skills().iter().map(|skill| skill.name.as_str()));
        let bare = Regex::new(&format!(r"`({names})(?:`| [^`]*`)( props?\b)?")).unwrap();
        let found: Vec<String> = checks::lines_naming(library(), &bare, &[], unread)
            .into_iter()
            .filter(|line| {
                bare.captures_iter(line)
                    .any(|capture| capture.get(2).is_none())
            })
            .collect();
        assert!(found.is_empty(), "{UNSCOPED_MSG}:\n{}", found.join("\n"));
    }

    #[test]
    fn no_page_the_model_reads_loads_a_user_only_skill() {
        let user_only: Vec<&str> = library()
            .skills()
            .iter()
            .filter(|skill| skill.user_only)
            .map(|skill| skill.name.as_str())
            .collect();
        let loaded = Regex::new(&format!(
            "`{NAME}/({})`",
            alternation(user_only.iter().copied())
        ))
        .unwrap();
        let owned_by_user_only = |page: &str| {
            user_only
                .iter()
                .any(|name| page.starts_with(&format!("{SKILLS_DIR}/{name}/")))
        };
        let found = checks::lines_naming(library(), &loaded, &[], |page| {
            unread(page) || owned_by_user_only(page)
        });
        assert!(
            found.is_empty(),
            "{LOADS_USER_ONLY_MSG}:\n{}",
            found.join("\n")
        );
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
        assert!(!library().skill(ANIMATE).unwrap().user_only);
        for name in USER_ONLY {
            assert!(library().skill(name).unwrap().user_only, "{name}");
        }
    }

    #[test]
    fn the_visual_check_page_is_served() {
        let loaded = library().load(&format!("/{VISUAL_CHECK}")).unwrap();
        assert!(loaded.page.is_some());
    }
}
