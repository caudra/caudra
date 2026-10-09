//! Links the bundled Rhai grammar into two-face's syntax set once, at build
//! time. Rebuilding the set on first use cost seconds in debug builds and
//! delayed every first highlight.

use std::env;
use std::path::PathBuf;

use syntect::dumps::dump_to_uncompressed_file;
use syntect::parsing::SyntaxDefinition;

const RHAI_SYNTAX_PATH: &str = "syntaxes/rhai.sublime-syntax";
const SYNTAX_DUMP: &str = "syntaxes.packdump";

fn main() {
    println!("cargo:rerun-if-changed={RHAI_SYNTAX_PATH}");
    let source = std::fs::read_to_string(RHAI_SYNTAX_PATH)
        .unwrap_or_else(|error| panic!("cannot read {RHAI_SYNTAX_PATH}: {error}"));
    let rhai = SyntaxDefinition::load_from_str(&source, true, None)
        .unwrap_or_else(|error| panic!("bundled Rhai grammar does not load: {error}"));
    let mut builder = two_face::syntax::extra_newlines().into_builder();
    builder.add(rhai);
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dump = out_dir.join(SYNTAX_DUMP);
    dump_to_uncompressed_file(&builder.build(), &dump)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", dump.display()));
}
