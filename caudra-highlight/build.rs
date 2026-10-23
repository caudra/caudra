//! Links the bundled grammars into two-face's syntax set once, at build
//! time. Rebuilding the set on first use cost seconds in debug builds and
//! delayed every first highlight.

use std::env;
use std::path::PathBuf;

use syntect::dumps::dump_to_uncompressed_file;
use syntect::parsing::SyntaxDefinition;

const SYNTAX_PATHS: &[&str] = &[
    "syntaxes/rhai.sublime-syntax",
    "syntaxes/tlaplus.sublime-syntax",
    "syntaxes/pluscal.sublime-syntax",
    "syntaxes/tlaplus-cfg.sublime-syntax",
];
const SYNTAX_DUMP: &str = "syntaxes.packdump";

fn main() {
    let mut builder = two_face::syntax::extra_newlines().into_builder();
    for path in SYNTAX_PATHS {
        println!("cargo:rerun-if-changed={path}");
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
        let syntax = SyntaxDefinition::load_from_str(&source, true, None)
            .unwrap_or_else(|error| panic!("bundled grammar {path} does not load: {error}"));
        builder.add(syntax);
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dump = out_dir.join(SYNTAX_DUMP);
    dump_to_uncompressed_file(&builder.build(), &dump)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", dump.display()));
}
