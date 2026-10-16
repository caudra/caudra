use std::fmt::Write;
use std::fs;

use caudra_docs::{ReleaseNotes, SITE_DOCS_URL};

use crate::{page_header, repository_path};

pub const SLUG: &str = "release-notes";
const NOTES_DIR: &str = "release-notes";
const TITLE: &str = "Release notes";
const DESCRIPTION: &str = "What changed in each Caudra release, newest first.";
const INTRO: &str = "\
Each release lists what changed, newest first. After an update, the first interactive start shows the notes for \
every release since the one you ran before. Run `/changelog` to read them again, or `/changelog all` for every \
release. [`ui.changelog_on_update`](/docs/configuration/#ui-changelog-on-update) turns the automatic view off.

To install Caudra, follow the [Quick Start](/docs/quick-start/). To update, run \
[`caudra update`](/docs/cli/#caudra-update-caudra-rollback). Each release links to its GitHub page, which holds \
the downloads.
";
/// Links into the docs stay on whichever site shows the page, as in the hand-written pages.
const RELATIVE_DOCS_LINK: &str = "](/docs/";

/// The notes under `release-notes/`, which are also the GitHub release bodies and what `/changelog` shows.
pub fn generate() -> String {
    let directory = repository_path(NOTES_DIR);
    let files: Vec<(String, String)> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .map(|entry| {
            let path = entry
                .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
                .path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_else(|| panic!("{}: not a UTF-8 name", path.display()))
                .to_owned();
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            (name, text)
        })
        .collect();
    let notes = ReleaseNotes::parse(
        files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str())),
    )
    .unwrap_or_else(|error| panic!("{}: {error}", directory.display()));
    page(&notes)
}

fn page(notes: &ReleaseNotes) -> String {
    let site_link = format!("]({SITE_DOCS_URL}");
    let mut out = page_header(TITLE, DESCRIPTION);
    out.push_str(INTRO);
    for release in notes.releases() {
        write!(
            out,
            "\n## {} {{#{}}}\n\n{}",
            release.title,
            release.anchor(),
            release.body().replace(&site_link, RELATIVE_DOCS_LINK)
        )
        .unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use caudra_docs::ReleaseNotes;
    use test_case::test_case;

    use super::page;

    const OLDER: (&str, &str) = (
        "0.2.0-preview.3.md",
        "- Preview 3 fixes things.\n\n### Fixes\n\n- **A fix.** Details.\n",
    );
    const NEWER: (&str, &str) = (
        "0.2.0-preview.4.md",
        "- Preview 4 adds things, see [Permissions](https://caudra.ai/docs/permissions/#modes).\n\n> [!IMPORTANT]\n> **Restart.** Now.\n\n### Features\n\n- **A feature.** Details.\n",
    );
    const NEWER_HEADING: &str = "## Caudra 0.2 Preview 4 {#v0-2-0-preview-4}";
    const OLDER_HEADING: &str = "## Caudra 0.2 Preview 3 {#v0-2-0-preview-3}";
    const GITHUB_LINK: &str = "[v0.2.0-preview.4 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.4)";
    const RELATIVE_LINK: &str = "[Permissions](/docs/permissions/#modes)";
    const ALERT: &str = "> **Important**\n>\n> **Restart.** Now.";
    const FRONT_MATTER: &str = "---\ntitle: \"Release notes\"\n";
    const ABSOLUTE_DOCS_LINK: &str = "](https://caudra.ai/docs/";

    fn fixture() -> String {
        page(&ReleaseNotes::parse([OLDER, NEWER]).unwrap())
    }

    #[test_case(FRONT_MATTER ; "front_matter")]
    #[test_case(NEWER_HEADING ; "explicit_id")]
    #[test_case(GITHUB_LINK ; "github_link_stays_absolute")]
    #[test_case(RELATIVE_LINK ; "docs_link_becomes_relative")]
    #[test_case(ALERT ; "alert_becomes_a_bold_label")]
    fn the_page_holds(expected: &str) {
        let page = fixture();
        assert!(page.contains(expected), "{page}");
    }

    #[test]
    fn releases_are_newest_first() {
        let page = fixture();
        let newer = page.find(NEWER_HEADING).unwrap();
        let older = page.find(OLDER_HEADING).unwrap();
        assert!(newer < older, "{page}");
    }

    #[test]
    fn no_absolute_docs_link_is_left_and_the_page_ends_with_one_newline() {
        let page = fixture();
        assert!(!page.contains(ABSOLUTE_DOCS_LINK), "{page}");
        assert!(page.ends_with('\n') && !page.ends_with("\n\n"), "{page:?}");
    }
}
