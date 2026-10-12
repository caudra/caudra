//! The release notes under `release-notes/`, one `<version>.md` file per release, as Caudra shows them after an
//! update and in `/changelog`. The same files are the GitHub release bodies, and the release pipeline adds the
//! install and update steps there, so a file holds only what changed: a lead paragraph, alerts for anything a
//! user must do, and `###` groups.

use std::cmp::Ordering;

use semver::Version;

use crate::SourceError;
use crate::markdown::{Fences, atx_heading};
use crate::slug::slugify;

pub const RELEASES_URL: &str = "https://github.com/caudra/caudra/releases/tag/";
const GITHUB_LINK: &str = " on GitHub";
const NOTES_EXTENSION: &str = ".md";
const TAG_PREFIX: &str = "v";
const TITLE_PREFIX: &str = "Caudra ";
const PREVIEW_PREFIX: &str = "preview.";
const PREVIEW_LABEL: &str = " Preview ";
/// The highest heading a release may use: the modal puts each release under a heading one level up.
const GROUP_LEVEL: u8 = 3;
const QUOTE: char = '>';
const ALERT_OPEN: &str = "[!";
const ALERT_CLOSE: char = ']';
/// GitHub's alert kinds, and the word each one shows in the TUI, which renders no alerts.
const ALERT_KINDS: [(&str, &str); 5] = [
    ("NOTE", "Note"),
    ("TIP", "Tip"),
    ("IMPORTANT", "Important"),
    ("WARNING", "Warning"),
    ("CAUTION", "Caution"),
];
/// Sections the release pipeline writes for GitHub readers, which someone already running the release does not need.
const GENERATED_HEADINGS: [&str; 2] = ["new to caudra?", "updating"];
/// What can open a Markdown block other than a paragraph.
const BLOCK_MARKERS: [&str; 9] = ["#", ">", "- ", "* ", "+ ", "|", "<", "    ", "\t"];

/// How the TUI reaches the notes the binary parses on first use.
pub type ReleaseNotesSource = fn() -> &'static ReleaseNotes;

/// Every release with notes, newest first.
#[derive(Debug)]
pub struct ReleaseNotes {
    releases: Vec<Release>,
}

#[derive(Debug)]
pub struct Release {
    pub version: Version,
    /// The git tag, such as `v0.2.0-preview.4`.
    pub tag: String,
    /// The GitHub release title, such as `Caudra 0.2 Preview 4`.
    pub title: String,
    pub url: String,
    /// The notes as the TUI renders them, with alerts spelled as bold labels.
    pub display: String,
}

impl ReleaseNotes {
    /// Validates `<version>.md` sources against the authoring rules in CONTRIBUTING.md.
    pub fn parse<'a>(
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, SourceError> {
        let mut releases = files
            .into_iter()
            .map(|(path, text)| Release::parse(path, text))
            .collect::<Result<Vec<_>, _>>()?;
        releases.sort_by(|a, b| b.version.cmp_precedence(&a.version));
        if let Some(pair) = releases
            .windows(2)
            .find(|pair| pair[0].version.cmp_precedence(&pair[1].version) == Ordering::Equal)
        {
            return Err(SourceError::new(
                &format!("{}{NOTES_EXTENSION}", pair[1].version),
                format!("duplicate notes for {}", pair[0].version),
            ));
        }
        Ok(Self { releases })
    }

    pub fn releases(&self) -> &[Release] {
        &self.releases
    }

    /// The releases a user updating from `since` to `current` has not seen: newer than `since`, up to and
    /// including `current`, newest first, skipped versions included. Without a `since` it is `current` alone. A
    /// `since` at or above `current`, as after a rollback, leaves nothing.
    pub fn since(&self, since: Option<&str>, current: &str) -> &[Release] {
        let shown = self.up_to(current);
        let end = match since.and_then(|since| Version::parse(since).ok()) {
            Some(since) => shown
                .iter()
                .position(|release| release.version.cmp_precedence(&since).is_le())
                .unwrap_or(shown.len()),
            None => match Version::parse(current) {
                Ok(current) => shown
                    .first()
                    .filter(|release| release.version.cmp_precedence(&current).is_eq())
                    .map_or(0, |_| 1),
                Err(_) => 0,
            },
        };
        &shown[..end]
    }

    /// Every release up to and including `current`, so notes written ahead of a release stay out of the build
    /// that precedes it.
    pub fn up_to(&self, current: &str) -> &[Release] {
        let Ok(current) = Version::parse(current) else {
            return &self.releases;
        };
        let start = self
            .releases
            .iter()
            .position(|release| release.version.cmp_precedence(&current).is_le())
            .unwrap_or(self.releases.len());
        &self.releases[start..]
    }
}

impl Release {
    fn parse(path: &str, text: &str) -> Result<Self, SourceError> {
        let version = path
            .strip_suffix(NOTES_EXTENSION)
            .and_then(|stem| Version::parse(stem).ok())
            .ok_or_else(|| {
                SourceError::new(
                    path,
                    "expected a <version>.md filename, such as 0.2.0-preview.4.md",
                )
            })?;
        let tag = format!("{TAG_PREFIX}{version}");
        Ok(Self {
            title: title(&version),
            url: format!("{RELEASES_URL}{tag}"),
            display: display(path, text)?,
            tag,
            version,
        })
    }

    /// The release as Caudra shows it under its heading, in `/changelog` and on the Release notes docs page: a
    /// link to its GitHub page, then the notes.
    pub fn body(&self) -> String {
        format!(
            "[{}{GITHUB_LINK}]({})\n\n{}",
            self.tag, self.url, self.display
        )
    }

    /// The id of the release's heading on the docs page, from the version, because titles hold dots that the
    /// site and the TUI slug differently.
    pub fn anchor(&self) -> String {
        slugify(&self.tag)
    }
}

/// The GitHub release title, as the release pipeline writes it.
fn title(version: &Version) -> String {
    let preview = version
        .pre
        .as_str()
        .strip_prefix(PREVIEW_PREFIX)
        .filter(|number| {
            !number.starts_with('0')
                && !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit())
        });
    match preview {
        Some(number) if version.patch == 0 => format!(
            "{TITLE_PREFIX}{}.{}{PREVIEW_LABEL}{number}",
            version.major, version.minor
        ),
        Some(number) => format!(
            "{TITLE_PREFIX}{}.{}.{}{PREVIEW_LABEL}{number}",
            version.major, version.minor, version.patch
        ),
        None => format!("{TITLE_PREFIX}{version}"),
    }
}

/// Checks the authoring rules and spells each alert marker as a bold label.
fn display(path: &str, text: &str) -> Result<String, SourceError> {
    let text = text.trim();
    let lead = text.lines().next().unwrap_or_default();
    if lead.is_empty() || BLOCK_MARKERS.iter().any(|marker| lead.starts_with(marker)) {
        return Err(SourceError::new(
            path,
            "start with a lead paragraph that summarizes the release",
        ));
    }
    let mut display = String::with_capacity(text.len());
    let mut fences = Fences::default();
    for line in text.lines() {
        if fences.step(line) {
            display.push_str(line);
        } else if let Some((level, heading)) = atx_heading(line) {
            check_heading(path, level, heading)?;
            display.push_str(line);
        } else if let Some(kind) = alert(line) {
            let label = ALERT_KINDS
                .iter()
                .find(|(name, _)| *name == kind)
                .map(|(_, label)| label)
                .ok_or_else(|| {
                    SourceError::new(
                        path,
                        format!("unknown alert `[!{kind}]`; use NOTE, TIP, IMPORTANT, WARNING or CAUTION"),
                    )
                })?;
            // A paragraph of its own, or the label runs into the first sentence wherever the quote reflows.
            display.push_str(&format!("{QUOTE} **{label}**\n{QUOTE}"));
        } else {
            display.push_str(line);
        }
        display.push('\n');
    }
    Ok(display)
}

fn check_heading(path: &str, level: u8, heading: &str) -> Result<(), SourceError> {
    if level < GROUP_LEVEL {
        return Err(SourceError::new(
            path,
            format!("group changes under ### headings, not `{heading}`"),
        ));
    }
    let lower = heading.to_lowercase();
    if GENERATED_HEADINGS.iter().any(|generated| {
        lower
            .strip_prefix(generated)
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric()))
    }) {
        return Err(SourceError::new(
            path,
            format!("leave out `{heading}`: the release pipeline adds install and update steps"),
        ));
    }
    Ok(())
}

/// The kind named by a `> [!KIND]` line, which GitHub shows as an alert.
fn alert(line: &str) -> Option<&str> {
    line.trim_start()
        .strip_prefix(QUOTE)?
        .trim()
        .strip_prefix(ALERT_OPEN)?
        .strip_suffix(ALERT_CLOSE)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use semver::Version;
    use test_case::test_case;

    use super::{Release, ReleaseNotes, title};
    use crate::fixture::leak;

    const LEAD: &str = "A summary.\n\n";
    const PREVIEW_3: &str = "0.2.0-preview.3";
    const PREVIEW_4: &str = "0.2.0-preview.4";
    const PREVIEW_6: &str = "0.2.0-preview.6";
    const LEAD_ERROR: &str = "lead paragraph";
    const GROUP_ERROR: &str = "### headings";
    const GENERATED_ERROR: &str = "release pipeline adds";
    const ALERT_ERROR: &str = "unknown alert";
    const NAME_ERROR: &str = "<version>.md";
    const DUPLICATE_ERROR: &str = "duplicate notes";
    const NOTES_DIR: &str = "../release-notes";
    const WORKSPACE_VERSION: &str = env!("CARGO_PKG_VERSION");
    const DRAFTS_AHEAD: usize = 1;

    fn notes(versions: &[&str]) -> ReleaseNotes {
        ReleaseNotes::parse(versions.iter().map(|version| {
            (
                leak(format!("{version}.md")),
                leak(format!("{LEAD}### Changes\n\n- **In {version}.** Done.\n")),
            )
        }))
        .unwrap()
    }

    fn versions(releases: &[Release]) -> Vec<String> {
        releases
            .iter()
            .map(|release| release.version.to_string())
            .collect()
    }

    #[test_case("0.2.0-preview.4.md", "### Groups\n", Some(LEAD_ERROR) ; "group_first")]
    #[test_case("0.2.0-preview.4.md", "> [!IMPORTANT]\n> Restart.\n", Some(LEAD_ERROR) ; "alert_first")]
    #[test_case("0.2.0-preview.4.md", "- **A.** B.\n", Some(LEAD_ERROR) ; "bullet_first")]
    #[test_case("0.2.0-preview.4.md", "  \n", Some(LEAD_ERROR) ; "empty")]
    #[test_case("0.2.0-preview.4.md", "A summary.\n\n## Changes\n", Some(GROUP_ERROR) ; "h2")]
    #[test_case("0.2.0-preview.4.md", "A summary.\n\n# Caudra\n", Some(GROUP_ERROR) ; "h1")]
    #[test_case("0.2.0-preview.4.md", "A summary.\n\n### New to Caudra?\n", Some(GENERATED_ERROR) ; "new_users")]
    #[test_case("0.2.0-preview.4.md", "A summary.\n\n### Updating from Preview 3\n", Some(GENERATED_ERROR) ; "updating")]
    #[test_case("0.2.0-preview.4.md", "A summary.\n\n> [!DANGER]\n> Stop.\n", Some(ALERT_ERROR) ; "unknown_alert")]
    #[test_case("preview-4.md", "A summary.\n", Some(NAME_ERROR) ; "not_a_version")]
    #[test_case("0.2.0-preview.4.txt", "A summary.\n", Some(NAME_ERROR) ; "not_markdown")]
    #[test_case("0.2.0-preview.4.md", "A summary.\n\n### Updates\n\n```md\n## Updating\n```\n", None ; "fenced_and_similar_headings_pass")]
    fn notes_follow_the_authoring_rules(
        path: &'static str,
        text: &'static str,
        error: Option<&str>,
    ) {
        match (ReleaseNotes::parse([(path, text)]), error) {
            (Ok(_), None) => {}
            (Err(found), Some(expected)) => assert!(found.message.contains(expected), "{found}"),
            (result, expected) => panic!("expected {expected:?}, got {result:?}"),
        }
    }

    #[test]
    fn two_files_for_one_version_are_refused() {
        let error = ReleaseNotes::parse([
            ("1.0.0.md", "A summary.\n"),
            ("1.0.0+build.md", "A summary.\n"),
        ])
        .unwrap_err();
        assert!(error.message.contains(DUPLICATE_ERROR), "{error}");
    }

    #[test]
    fn releases_are_newest_first_by_precedence() {
        let notes = notes(&["0.2.0-preview.9", "0.2.0", "0.2.0-preview.10", "0.1.0"]);
        assert_eq!(
            versions(notes.releases()),
            ["0.2.0", "0.2.0-preview.10", "0.2.0-preview.9", "0.1.0"]
        );
    }

    #[test_case(Some("0.2.0-preview.2"), PREVIEW_4 => vec![PREVIEW_4, PREVIEW_3] ; "skipped_versions")]
    #[test_case(Some(PREVIEW_3), PREVIEW_4 => vec![PREVIEW_4] ; "one_step")]
    #[test_case(None, PREVIEW_4 => vec![PREVIEW_4] ; "no_since")]
    #[test_case(Some(PREVIEW_4), PREVIEW_4 => Vec::<&str>::new() ; "same_version")]
    #[test_case(Some(PREVIEW_6), PREVIEW_4 => Vec::<&str>::new() ; "rollback")]
    #[test_case(Some("0.1.0"), PREVIEW_4 => vec![PREVIEW_4, PREVIEW_3, "0.2.0-preview.2"] ; "since_without_notes")]
    #[test_case(Some("garbage"), PREVIEW_4 => vec![PREVIEW_4] ; "unparsable_since")]
    #[test_case(None, "0.2.0-preview.5" => Vec::<&str>::new() ; "current_without_notes")]
    #[test_case(Some(PREVIEW_3), "0.2.0-preview.5" => vec![PREVIEW_4] ; "current_without_notes_keeps_older")]
    fn since_spans_what_the_user_has_not_seen(since: Option<&str>, current: &str) -> Vec<String> {
        let notes = notes(&["0.2.0-preview.2", PREVIEW_3, PREVIEW_4, PREVIEW_6]);
        versions(notes.since(since, current))
    }

    #[test]
    fn up_to_leaves_out_notes_written_ahead() {
        let notes = notes(&[PREVIEW_3, PREVIEW_4, PREVIEW_6]);
        assert_eq!(versions(notes.up_to(PREVIEW_4)), [PREVIEW_4, PREVIEW_3]);
    }

    #[test_case("0.2.0-preview.4" => "Caudra 0.2 Preview 4" ; "preview")]
    #[test_case("0.2.1-preview.2" => "Caudra 0.2.1 Preview 2" ; "patch_preview")]
    #[test_case("0.2.1" => "Caudra 0.2.1" ; "stable")]
    #[test_case("0.2.0-rc.1" => "Caudra 0.2.0-rc.1" ; "other_prerelease")]
    #[test_case("0.2.0-preview.0" => "Caudra 0.2.0-preview.0" ; "preview_zero")]
    #[test_case("0.2.0-preview.1.2" => "Caudra 0.2.0-preview.1.2" ; "preview_with_more_identifiers")]
    fn titles_match_the_release_pipeline(version: &str) -> String {
        title(&Version::parse(version).unwrap())
    }

    /// Every file in `release-notes/`, read from disk.
    fn checked_in_notes() -> ReleaseNotes {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(NOTES_DIR);
        let files = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    leak(entry.file_name().into_string().unwrap()),
                    leak(fs::read_to_string(entry.path()).unwrap()),
                )
            })
            .collect::<Vec<_>>();
        assert!(!files.is_empty(), "{}", directory.display());
        ReleaseNotes::parse(files).unwrap()
    }

    #[test]
    fn checked_in_notes_follow_the_authoring_rules() {
        checked_in_notes();
    }

    /// Changes land in the next release's draft, so a second file ahead of the workspace version is a draft
    /// started in the wrong file.
    #[test]
    fn checked_in_notes_draft_only_the_next_release() {
        let notes = checked_in_notes();
        let ahead = notes.releases().len() - notes.up_to(WORKSPACE_VERSION).len();
        assert!(
            ahead <= DRAFTS_AHEAD,
            "release-notes/ drafts {:?} ahead of {WORKSPACE_VERSION}: merge them into the next release's file",
            versions(&notes.releases()[..ahead])
        );
    }

    #[test]
    fn alerts_become_bold_labels_and_links_name_the_tag() {
        let notes = ReleaseNotes::parse([(
            "0.2.0-preview.4.md",
            "A summary.\n\n> [!IMPORTANT]\n> **Restart.** Now.\n\n```md\n> [!TIP]\n```\n",
        )])
        .unwrap();
        let release = &notes.releases()[0];
        assert_eq!(
            release.display,
            "A summary.\n\n> **Important**\n>\n> **Restart.** Now.\n\n```md\n> [!TIP]\n```\n"
        );
        assert_eq!(release.tag, "v0.2.0-preview.4");
        assert_eq!(
            release.url,
            "https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.4"
        );
        assert!(release.body().starts_with(
            "[v0.2.0-preview.4 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.4)\n\nA summary."
        ));
        assert_eq!(release.anchor(), "v0-2-0-preview-4");
    }
}
