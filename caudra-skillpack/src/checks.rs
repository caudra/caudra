//! The invariants every pack's tests assert: what `make sync-<pack>` vendored is what its
//! manifest hashes, every translation follows its upstream file, and every served page reads as
//! Caudra's.

use std::collections::BTreeSet;

use include_dir::Dir;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Library, MANIFEST, MARKDOWN, TRANSLATIONS, files, frontmatter, normalize, skill_of};

const NAME_MISMATCH_MSG: &str = "a skill's frontmatter name must be its directory";
const DANGLING_MSG: &str = "a relative link must resolve to a page";
const ORPHAN_MSG: &str = "a translation's upstream file is gone; delete or move it";
const UNLISTED_MSG: &str = "translations.json must list exactly the translated files";
const BARE_COMMAND_MSG: &str = "a command the user types carries the pack's scope";

pub fn every_skill_is_named_by_its_directory(library: &Library) {
    for (path, text) in library.pages() {
        if let Some(directory) = skill_of(path) {
            assert_eq!(
                frontmatter(text).name.as_deref(),
                Some(directory),
                "{NAME_MISMATCH_MSG}: {path}"
            );
        }
    }
}

/// `placeholders` are links upstream writes as templates for the model to fill in.
pub fn every_relative_link_resolves(library: &Library, placeholders: &[&str]) {
    let link = Regex::new(r"\]\(([^)\s]+)\)").unwrap();
    for (path, text) in library.pages() {
        if !path.ends_with(MARKDOWN) {
            continue;
        }
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        for capture in link.captures_iter(text) {
            let href = capture[1].split(['#', '?']).next().unwrap_or_default();
            if href.is_empty() || href.contains(':') || placeholders.contains(&href) {
                continue;
            }
            let target = normalize(&format!("{dir}/{href}")).unwrap();
            let found = library.page(&target).is_some() || library.listing(&target).is_some();
            assert!(found, "{DANGLING_MSG}: {path} -> {href}");
        }
    }
}

fn json(dir: &Dir<'static>, file: &str) -> Value {
    serde_json::from_str(dir.get_file(file).unwrap().contents_utf8().unwrap()).unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn the_manifest_hashes_every_vendored_file(library: &Library, upstream: &'static Dir<'static>) {
    let drift = format!(
        "upstream.json must hash every vendored file; run `make sync-{}`",
        library.name()
    );
    let manifest = json(upstream, MANIFEST);
    let listed: BTreeSet<String> = manifest["files"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let mut vendored = BTreeSet::new();
    for file in files(upstream) {
        let path = file.path().to_str().unwrap();
        if path == MANIFEST {
            continue;
        }
        assert_eq!(
            manifest["files"][path],
            sha256(file.contents()),
            "{drift}: {path}"
        );
        vendored.insert(path.to_owned());
    }
    assert_eq!(vendored, listed, "{drift}");
}

/// The upstream hash each translation was made from must still be upstream's, so a sync that
/// changes a translated file fails here until the translation follows it.
pub fn every_translation_follows_its_upstream_file(
    library: &Library,
    upstream: &'static Dir<'static>,
    translated: &'static Dir<'static>,
) {
    let stale = format!(
        "upstream changed since this translation; retranslate it, then \
         `make sync-{} ARGS='--mark <path>'` it",
        library.name()
    );
    let manifest = json(upstream, MANIFEST);
    let recorded = json(translated, TRANSLATIONS);
    let recorded = recorded["files"].as_object().unwrap();
    let translations: BTreeSet<&str> = files(translated)
        .into_iter()
        .filter_map(|file| file.path().to_str())
        .filter(|path| *path != TRANSLATIONS)
        .collect();
    for path in &translations {
        let base = &manifest["files"][*path];
        assert!(!base.is_null(), "{ORPHAN_MSG}: {path}");
        assert_eq!(recorded.get(*path), Some(base), "{stale}: {path}");
    }
    let listed: BTreeSet<&str> = recorded.keys().map(String::as_str).collect();
    assert_eq!(listed, translations, "{UNLISTED_MSG}");
}

/// Every line of a served page that matches `term`, as `page:line: text`. `skipped` pages are not
/// read, and `allowed` pairs a page with a term it names on purpose.
pub fn lines_naming(
    library: &Library,
    term: &Regex,
    allowed: &[(&str, &str)],
    skipped: impl Fn(&str) -> bool,
) -> Vec<String> {
    library
        .pages()
        .iter()
        .filter(|(page, _)| !skipped(page))
        .filter(|(page, _)| {
            !allowed
                .iter()
                .any(|(allowed, pattern)| allowed == *page && *pattern == term.as_str())
        })
        .flat_map(|(page, text)| {
            text.lines()
                .enumerate()
                .filter(|(_, line)| term.is_match(line))
                .map(move |(number, line)| format!("{page}:{}: {}", number + 1, line.trim()))
        })
        .collect()
}

/// No served page names one of `terms`, the other harness's vocabulary that the pack's
/// translation rewrites.
pub fn no_page_names_a_foreign_term(
    library: &Library,
    terms: &[&str],
    allowed: &[(&str, &str)],
    skipped: impl Fn(&str) -> bool,
) {
    let found: Vec<String> = terms
        .iter()
        .flat_map(|pattern| lines_naming(library, &Regex::new(pattern).unwrap(), allowed, &skipped))
        .collect();
    assert!(
        found.is_empty(),
        "these lines still name another harness's vocabulary; translate them with the \
         skillpack-translate skill:\n{}",
        found.join("\n")
    );
}

/// A skill the user runs is `/<pack>:<skill>`, never the bare `/<skill>` upstream writes.
pub fn every_skill_command_carries_the_scope(library: &Library, skipped: impl Fn(&str) -> bool) {
    let names: Vec<String> = library
        .skills()
        .iter()
        .map(|skill| regex::escape(&skill.name))
        .collect();
    let bare = Regex::new(&format!(
        r"(?m)(^|[\s`(\[*'])/({})($|[^\w-])",
        names.join("|")
    ))
    .unwrap();
    let found = lines_naming(library, &bare, &[], skipped);
    assert!(
        found.is_empty(),
        "{BARE_COMMAND_MSG} `/{}:<name>`:\n{}",
        library.name(),
        found.join("\n")
    );
}
