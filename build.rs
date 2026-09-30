const DOCS_CONTENT: &str = "site/docs/content";

/// `include_dir!` in `src/docs.rs` embeds the docs, but Cargo does not know those files feed the build, so an
/// added or removed page would not rebuild the binary. Watching the directory covers every file under it.
fn main() {
    println!("cargo:rerun-if-changed={DOCS_CONTENT}");
}
