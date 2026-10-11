//! The release notes under `release-notes`, embedded like the docs so `/changelog` works offline and shows exactly
//! the notes this build shipped with.

use std::sync::LazyLock;

use caudra_docs::ReleaseNotes;
use include_dir::{Dir, include_dir};

static CONTENT: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/release-notes");
static NOTES: LazyLock<ReleaseNotes> = LazyLock::new(|| {
    ReleaseNotes::parse(
        CONTENT
            .files()
            .filter_map(|file| Some((file.path().to_str()?, file.contents_utf8()?))),
    )
    .expect("valid embedded release notes")
});

/// Parsed on first use, so a session that never shows them never pays for them.
pub fn notes() -> &'static ReleaseNotes {
    &NOTES
}

#[cfg(test)]
mod tests {
    use super::notes;

    const CURRENT: &str = env!("CARGO_PKG_VERSION");

    #[test]
    fn embedded_notes_cover_this_version() {
        assert!(
            notes()
                .releases()
                .iter()
                .any(|release| release.version.to_string() == CURRENT),
            "release-notes/{CURRENT}.md is missing"
        );
    }
}
