//! Concatenates the vendored rule files into one TOML document.
//!
//! Concatenation happens at build time so a corpus refresh cannot introduce a
//! duplicate rule name or a syntax error without failing the build. The runtime
//! then parses a single embedded document instead of walking a directory, which
//! keeps rule loading independent of the filesystem the server is pointed at.

use std::collections::BTreeMap;
use std::path::Path;
use std::{env, fs};

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    // `rules` is vendored upstream and must stay byte-identical so a refresh is
    // a clean copy. `rules-workcell` holds rules this project authors. Keeping
    // them in separate directories keeps that provenance boundary visible.
    let vendored = Path::new(&manifest).join("rules");
    let authored = Path::new(&manifest).join("rules-workcell");
    println!("cargo::rerun-if-changed=rules");
    println!("cargo::rerun-if-changed=rules-workcell");

    // Sorted by file name so first-match-wins resolution is deterministic and
    // reproducible across platforms with different directory ordering.
    let mut sources = BTreeMap::new();
    for directory in [&vendored, &authored] {
        for entry in fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        {
            let path = entry.expect("rule directory entry").path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let name = path
                .file_name()
                .expect("rule file name")
                .to_str()
                .expect("rule file name is UTF-8")
                .to_owned();
            let contents = fs::read_to_string(&path).expect("read rule file");
            // A shared file name across the two directories would silently drop
            // one of them, so collision is a build failure rather than a
            // last-writer-wins merge.
            assert!(
                sources.insert(name.clone(), contents).is_none(),
                "rule file `{name}` exists in both rules/ and rules-workcell/"
            );
        }
    }

    if sources.is_empty() {
        panic!(
            "no rule files found in {} or {}",
            vendored.display(),
            authored.display()
        );
    }

    let mut document = String::new();
    for contents in sources.values() {
        document.push_str(contents);
        document.push('\n');
    }

    // Parsing here turns a malformed or duplicated refresh into a build failure
    // with the offending name, rather than a runtime error on a live server.
    let parsed: toml::Value = match toml::from_str(&document) {
        Ok(value) => value,
        Err(error) => panic!("rule corpus is not valid TOML: {error}"),
    };
    let filters = parsed
        .get("filters")
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("rule corpus has no [filters] table"));
    if filters.is_empty() {
        panic!("rule corpus defines no filters");
    }
    for name in filters.keys() {
        let count = sources
            .values()
            .filter(|contents| contents.contains(&format!("[filters.{name}]")))
            .count();
        assert!(
            count <= 1,
            "rule `{name}` is defined in {count} files; names must be unique"
        );
    }

    let out = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("rules.toml");
    fs::write(&out, document).expect("write concatenated rules");
}
