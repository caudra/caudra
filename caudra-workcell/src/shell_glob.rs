//! The globs a rule can review as written: literal names with `*`, `?` and
//! bracket classes, expanded the way bash does with its default options.

use std::{
    fs,
    path::{Path, PathBuf},
};

use globset::Glob;

const GLOB_CHARACTERS: [char; 3] = ['*', '?', '['];
/// `!` negates a class, and outside one bash and globset both read it as
/// itself.
const WILDCARDS: &str = "*?!";
const LITERAL_PUNCTUATION: &str = "._/+@:,=-";
const FLAG_PREFIX: char = '-';
const CLASS_OPEN: char = '[';
const CLASS_CLOSE: char = ']';
const SEPARATOR: char = '/';
const FILESYSTEM_ROOT: &str = "/";
const HIDDEN_PREFIX: &str = ".";
const PARENT_DIRECTORY: &str = "..";
const MAX_SCANNED_ENTRIES: usize = 4096;
const MAX_MATCHES: usize = 1024;

/// Whether an unquoted word the shell globs is one a rule can review as
/// written: literal characters, wildcards and flat bracket classes, led by a
/// literal that is no flag, so no match can turn into one. `$`, quotes,
/// escapes, braces, `~`, parentheses and `^` all fail, since each makes the
/// text stand for something other than the names it matches.
pub(crate) fn safe_glob(word: &str) -> bool {
    let mut in_class = false;
    word.starts_with(|first: char| first != FLAG_PREFIX && literal(first))
        && word.contains(GLOB_CHARACTERS)
        && word.chars().all(|character| match character {
            CLASS_OPEN if !in_class => {
                in_class = true;
                true
            }
            CLASS_CLOSE if in_class => {
                in_class = false;
                true
            }
            SEPARATOR => !in_class,
            character => literal(character) || WILDCARDS.contains(character),
        })
        && !in_class
}

fn literal(character: char) -> bool {
    character.is_ascii_alphanumeric() || LITERAL_PUNCTUATION.contains(character)
}

/// The paths bash passes for `pattern` run from `workdir` with its default
/// options: no dotglob, so only a component that starts with `.` matches a
/// name that does, and no component crosses `/`. Each directory's matches come
/// in sorted order, and none at all means bash passes the pattern itself.
///
/// `None` when the expansion reads more than `MAX_SCANNED_ENTRIES` entries or
/// yields more than `MAX_MATCHES` paths, when globset cannot read a component,
/// or when a component matches `..`, which bash before 5.2 lists.
pub(crate) fn expand_glob(pattern: &str, workdir: &Path) -> Option<Vec<PathBuf>> {
    let start = if pattern.starts_with(SEPARATOR) {
        Path::new(FILESYSTEM_ROOT)
    } else {
        workdir
    };
    let mut budget = MAX_SCANNED_ENTRIES;
    pattern
        .split(SEPARATOR)
        .filter(|component| !component.is_empty())
        .try_fold(vec![start.to_path_buf()], |paths, component| {
            let expanded = if component.contains(GLOB_CHARACTERS) {
                matching_entries(component, &paths, &mut budget)?
            } else {
                paths
                    .into_iter()
                    .map(|path| path.join(component))
                    .filter(|path| path.symlink_metadata().is_ok())
                    .collect()
            };
            (expanded.len() <= MAX_MATCHES).then_some(expanded)
        })
}

/// The entries of `directories` one glob component matches, charging every
/// entry read to `budget`.
fn matching_entries(
    component: &str,
    directories: &[PathBuf],
    budget: &mut usize,
) -> Option<Vec<PathBuf>> {
    let matcher = Glob::new(component).ok()?.compile_matcher();
    let hidden = component.starts_with(HIDDEN_PREFIX);
    if hidden && matcher.is_match(PARENT_DIRECTORY) {
        return None;
    }
    let mut matches = Vec::new();
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        let mut names = Vec::new();
        for entry in entries {
            *budget = budget.checked_sub(1)?;
            names.push(entry.ok()?.file_name());
        }
        names.sort();
        matches.extend(
            names
                .into_iter()
                .filter(|name| {
                    (hidden
                        || !name
                            .as_encoded_bytes()
                            .starts_with(HIDDEN_PREFIX.as_bytes()))
                        && matcher.is_match(name)
                })
                .map(|name| directory.join(name)),
        );
    }
    Some(matches)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::{MAX_MATCHES, MAX_SCANNED_ENTRIES, expand_glob, safe_glob};

    const FIXTURE: &str = "fixture";

    #[test_case("src/*" => true; "a_directory_listing")]
    #[test_case("caudra-highlight/src/*.rs" => true; "a_suffix_under_literal_directories")]
    #[test_case("a?c" => true; "a_single_character")]
    #[test_case("src/[!a-c]*" => true; "a_negated_class")]
    #[test_case("src" => false; "no_wildcard")]
    #[test_case("*.rs" => false; "a_leading_wildcard")]
    #[test_case("-*" => false; "a_match_that_could_be_a_flag")]
    #[test_case("~/x*" => false; "a_home_directory")]
    #[test_case("src/$dir*" => false; "an_expansion")]
    #[test_case("src/{a,b}*" => false; "a_brace_expansion")]
    #[test_case(r"src/\**" => false; "an_escape")]
    #[test_case("src/'a'*" => false; "a_quote")]
    #[test_case("src/[[:alpha:]]*" => false; "a_nested_class")]
    #[test_case("src/[[:alpha:]*" => false; "a_class_opened_inside_a_class")]
    #[test_case("src/[a/b]*" => false; "a_separator_inside_a_class")]
    #[test_case("src/[ab*" => false; "an_unclosed_class")]
    #[test_case("src/a]*" => false; "a_stray_class_end")]
    fn a_safe_glob_is_literal_text_with_wildcards(word: &str) -> bool {
        safe_glob(word)
    }

    #[test_case("dir/*", Some(vec!["dir/a.rs", "dir/b.rs", "dir/sub"]); "a_star_skips_dotfiles_in_sorted_order")]
    #[test_case("dir/.h*", Some(vec!["dir/.hidden"]); "a_leading_dot_matches_dotfiles")]
    #[test_case("dir/?.rs", Some(vec!["dir/a.rs", "dir/b.rs"]); "a_question_mark_is_one_character")]
    #[test_case("dir/*/c.rs", Some(vec!["dir/sub/c.rs"]); "a_glob_descends_into_what_it_matched")]
    #[test_case("dir/*.md", Some(vec![]); "no_match_expands_to_nothing")]
    #[test_case("dir/.*", None; "a_component_matching_the_parent_is_refused")]
    fn a_glob_expands_like_bash(pattern: &str, expected: Option<Vec<&str>>) {
        let root = tempfile::tempdir().expect("root");
        let directory = root.path().join("dir");
        fs::create_dir_all(directory.join("sub")).expect("directories");
        for file in ["b.rs", "a.rs", ".hidden", "sub/c.rs"] {
            fs::write(directory.join(file), FIXTURE).expect("file");
        }

        assert_eq!(
            expand_glob(pattern, root.path()),
            expected.map(|paths| paths.iter().map(|path| root.path().join(path)).collect())
        );
    }

    #[test_case(MAX_MATCHES, "*" => Some(MAX_MATCHES); "every_match_within_the_bound")]
    #[test_case(MAX_MATCHES + 1, "*" => None; "one_match_past_the_bound")]
    #[test_case(MAX_SCANNED_ENTRIES, "f0*" => Some(1); "every_entry_within_the_scan_bound")]
    #[test_case(MAX_SCANNED_ENTRIES + 1, "f0*" => None; "one_entry_past_the_scan_bound")]
    fn an_expansion_is_bounded(entries: usize, pattern: &str) -> Option<usize> {
        let root = tempfile::tempdir().expect("root");
        for entry in 0..entries {
            fs::write(root.path().join(format!("f{entry}")), FIXTURE).expect("entry");
        }

        expand_glob(pattern, root.path()).map(|paths| paths.len())
    }
}
