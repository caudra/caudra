//! The files a Workcell patch declares.
//!
//! Advisory only: Workcell remains the sole parser and validator of a patch.
//! This scan names files for a header, for a stale-read notice on a patch that
//! never got far enough to report its own resources, and for the header a call
//! shows while its arguments are still arriving. All three read one parser, so
//! the row a patch streams is the row it settles on.

/// What every envelope line leads with.
pub const MARKER: &str = "*** ";
const VERBS: &[&str] = &["Add File:", "Update File:", "Delete File:", "Move to:"];
const WITHOUT_FILES: &str = "file patch";
/// Room a header spends naming files before it reports a count instead.
const HEADER_BUDGET: usize = 60;

pub fn paths(patch_text: &str) -> Vec<&str> {
    patch_text
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix(MARKER)?;
            VERBS
                .iter()
                .find_map(|verb| rest.strip_prefix(verb))
                .map(str::trim)
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// Names the files the patch declares, so the row reads like an edit's
/// instead of the same three words on every patch. The count stands in once
/// naming them all would cost more room than it earns.
pub fn header(patch_text: &str) -> String {
    let paths = paths(patch_text);
    match paths.len() {
        0 => WITHOUT_FILES.to_owned(),
        1 => paths[0].to_owned(),
        _ if paths.iter().map(|p| p.len() + 2).sum::<usize>() <= HEADER_BUDGET => paths.join(", "),
        n => format!("{n} files"),
    }
}

#[cfg(test)]
mod tests {
    use super::header;
    use test_case::test_case;

    const ONE_FILE: &str = "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** End Patch";
    const TWO_FILES: &str =
        "*** Begin Patch\n*** Update File: a.rs\n*** Delete File: b.rs\n*** End Patch";
    const NO_FILES: &str = "*** Begin Patch\n*** End Patch";

    #[test_case(ONE_FILE, "created.txt" ; "one_file_names_itself")]
    #[test_case(TWO_FILES, "a.rs, b.rs" ; "a_few_files_are_all_named")]
    #[test_case(NO_FILES, "file patch" ; "a_patch_naming_nothing_says_so")]
    fn a_patch_header_names_the_files_it_touches(patch_text: &str, expected: &str) {
        assert_eq!(header(patch_text), expected);
    }

    /// Naming every file stops paying once the row cannot hold them, so the
    /// count takes over rather than the header running off the screen.
    #[test]
    fn a_wide_patch_header_reports_a_count() {
        let patch = (0..9)
            .map(|i| format!("*** Update File: crates/some/deep/path/file_{i}.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(header(&patch), "9 files");
    }
}
