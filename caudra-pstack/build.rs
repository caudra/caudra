//! `include_dir!` embeds the files it finds when it expands, so a file added to one of these
//! trees would otherwise be missed until something else rebuilt the crate.

const EMBEDDED_TREES: [&str; 3] = ["upstream", "translated", "automations"];

fn main() {
    for tree in EMBEDDED_TREES {
        println!("cargo::rerun-if-changed={tree}");
    }
}
