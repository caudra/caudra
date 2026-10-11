const DOCS_CONTENT: &str = "docs/content";
const DOCS_NAVIGATION: &str = "docs/navigation.json";
const RELEASE_NOTES: &str = "release-notes";

/// `include_dir!` in `src/docs.rs` and `src/release_notes.rs` embeds the docs and the release notes, but Cargo does
/// not know those files feed the build, so an added or removed file would not rebuild the binary. Watching each
/// directory covers every file under it.
fn main() {
    println!("cargo:rerun-if-changed={DOCS_CONTENT}");
    println!("cargo:rerun-if-changed={DOCS_NAVIGATION}");
    println!("cargo:rerun-if-changed={RELEASE_NOTES}");
}
