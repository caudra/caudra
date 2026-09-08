//! Classifies shell commands that only observe, so planning can run them.
//!
//! The allowlist is deliberately small. Anything it does not recognize is not
//! refused, it is prompted, so the list never has to be exhaustive to be safe.

use std::path::{Component, Path};

use caudra_agent::permissions::physical_boundary_check;
use workcell::shell::ShellCommandAnalysis;

const GIT: &str = "git";
const GIT_READ_SUBCOMMANDS: &[&str] = &[
    "blame",
    "branch",
    "describe",
    "diff",
    "log",
    "ls-files",
    "ls-tree",
    "reflog",
    "rev-parse",
    "shortlog",
    "show",
    "status",
    "tag",
];
/// Every one of these redirects git at another repository, another program, or
/// another output file, which takes the call outside what its subcommand says.
const GIT_DENIED_FLAGS: &[&str] = &[
    "--config-env",
    "--exec-path",
    "--ext-diff",
    "--git-dir",
    "--output",
    "--upload-pack",
    "--work-tree",
    "-C",
    "-c",
];
const RG: &str = "rg";
/// Each of these makes ripgrep run another program or unpack an archive.
const RG_DENIED_FLAGS: &[&str] = &[
    "--hostname-bin",
    "--pre",
    "--pre-glob",
    "--search-zip",
    "-z",
];
const FIND: &str = "find";
/// Each of these makes find execute, delete, or write.
const FIND_DENIED_FLAGS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprintf", "-ok", "-okdir",
];
/// Substitution leaves the operand undecidable from the text at any position,
/// quoted or not, since `"$HOME"` expands exactly as `$HOME` does.
const SUBSTITUTES: &[char] = &['$', '`'];
/// Unquoted, these expand against the filesystem before the command runs.
const GLOBS: &[char] = &['*', '?', '['];
/// `sed` and `awk` are absent on purpose rather than deny-listed: `sed -i`
/// writes, `sed`'s `w` command writes from inside the script, and `awk` has
/// `system()` and `print >`. None can be made safe by rejecting flags.
const READ_ONLY_COMMANDS: &[&str] = &[
    "basename", "cat", "date", "df", "dirname", "du", "file", "head", "jq", "ls", "pwd",
    "readlink", "realpath", "stat", "tail", "tree", "uname", "wc", "which",
];

/// Reports whether every command in an analyzed line only observes.
///
/// `opaque` means the analysis could not account for part of the line, so the
/// reviewed text describes less than the command does and nothing about it can
/// be trusted.
pub(crate) fn is_read_only(analysis: &ShellCommandAnalysis, opaque: bool) -> bool {
    !opaque
        && !analysis.scopes.is_empty()
        && analysis
            .scopes
            .iter()
            .all(|scope| scope_is_read_only(&scope.normalized))
}

fn scope_is_read_only(normalized: &str) -> bool {
    let mut tokens = normalized.split_whitespace();
    let Some(executable) = tokens.next() else {
        return false;
    };
    let arguments: Vec<&str> = tokens.collect();
    match executable {
        GIT => {
            !denies(&arguments, GIT_DENIED_FLAGS)
                && arguments
                    .iter()
                    .find(|argument| !argument.starts_with('-'))
                    .is_some_and(|subcommand| {
                        GIT_READ_SUBCOMMANDS.contains(&unquote(subcommand).as_str())
                    })
        }
        RG => !denies(&arguments, RG_DENIED_FLAGS),
        FIND => !denies(&arguments, FIND_DENIED_FLAGS),
        _ => READ_ONLY_COMMANDS.contains(&executable),
    }
}

/// Reports whether a line can only reach inside the project.
///
/// A read-only command still reads, and the project's own read tools are bound
/// to the project, so granting one without the same bound would make `cat` reach
/// what `file_read` has to ask about. Confinement is decided from the text, so
/// anything it cannot resolve counts as escaping: an absolute path, a `~`, or
/// any `..` segment. The workdir is checked too, because Caudra runs the shell
/// unconfined and a relative operand is only inside when its base is.
pub(crate) fn stays_in_project(
    analysis: &ShellCommandAnalysis,
    workdir: &Path,
    project: &Path,
) -> bool {
    workdir.starts_with(project)
        && analysis
            .scopes
            .iter()
            .all(|scope| scope_stays_in_project(&scope.normalized, workdir, project))
}

fn scope_stays_in_project(normalized: &str, workdir: &Path, project: &Path) -> bool {
    normalized
        .split_whitespace()
        .all(|token| token_stays_in_project(token, workdir, project))
}

fn token_stays_in_project(token: &str, workdir: &Path, project: &Path) -> bool {
    // Judging a token's text only means anything while the text is the operand.
    // Workcell marks `${...}` and `$(...)` opaque, but a bare `$HOME` is a
    // `simple_expansion` it leaves intact, and `$HOME/.ssh/id_rsa` then reads as
    // an ordinary relative path. An unquoted glob names a set the text does not.
    if token.contains(SUBSTITUTES) || has_unquoted(token, GLOBS) {
        return false;
    }
    // A flag carrying an attached value hides a second operand, and
    // `--file=/etc/passwd` reads it just as surely as a bare path would.
    token.split('=').all(|part| {
        stays_inside(part)
            && stays_inside(&unquote(part))
            && resolves_inside(&unquote(part), workdir, project)
    })
}

/// Text cannot see through a symlink, and a project can contain one pointing
/// anywhere, so a token is resolved and bounded by the same symlink-aware check
/// the file tools use.
///
/// Most tokens are not paths at all, and they need no special case: the rules
/// above have already excluded every form that escapes, so a flag or a pattern
/// joins the workdir and canonicalizes to its own lexical form, which is inside.
/// Only a link can leave, and only resolving finds it.
fn resolves_inside(operand: &str, workdir: &Path, project: &Path) -> bool {
    physical_boundary_check(project, &workdir.join(operand)) == Some(true)
}

/// Quoting decides whether a glob expands, so this reads the token as the shell
/// would rather than stripping quotes first. A backslash escape is not honoured,
/// which only costs a prompt.
fn has_unquoted(token: &str, characters: &[char]) -> bool {
    let mut quote = None;
    for character in token.chars() {
        match quote {
            Some(open) if open == character => quote = None,
            Some(_) => {}
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if characters.contains(&character) => return true,
            None => {}
        }
    }
    false
}

fn stays_inside(operand: &str) -> bool {
    let path = Path::new(operand);
    !path.is_absolute()
        && !operand.starts_with('~')
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
}

fn denies(arguments: &[&str], denied: &[&str]) -> bool {
    arguments.iter().any(|argument| {
        let argument = unquote(argument);
        denied.iter().any(|flag| {
            argument == *flag
                || argument
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    })
}

/// Strips the quoting a shell would remove before the program sees the word, so
/// `"-exec"` and `-exe\c` cannot smuggle a denied flag past a literal compare.
/// Over-matching only costs a prompt, which is the safe direction.
fn unquote(argument: &str) -> String {
    argument
        .chars()
        .filter(|character| !matches!(character, '\'' | '"' | '\\'))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{is_read_only, stays_in_project};
    use test_case::test_case;
    use workcell::shell::{ShellCommandAnalysis, ShellCommandScope};

    fn analysis(commands: &[&str]) -> ShellCommandAnalysis {
        ShellCommandAnalysis {
            scopes: commands
                .iter()
                .map(|command| ShellCommandScope {
                    start_byte: 0,
                    source: (*command).into(),
                    normalized: (*command).into(),
                    permission: format!(
                        "{} *",
                        command.split_whitespace().next().unwrap_or_default()
                    ),
                })
                .collect(),
            opaque: false,
        }
    }

    #[test_case("git log --oneline -20" => true ; "git_log")]
    #[test_case("git status --short" => true ; "git_status")]
    #[test_case("git --no-pager diff" => true ; "a_harmless_global_flag_still_finds_the_subcommand")]
    #[test_case("git push origin main" => false ; "git_push_is_not_a_read_subcommand")]
    #[test_case("git -c core.pager=sh log" => false ; "git_dash_c_can_run_a_pager")]
    #[test_case("git -C /elsewhere log" => false ; "git_dash_big_c_leaves_the_repository")]
    #[test_case("git \"-C\" /elsewhere log" => false ; "quoting_does_not_hide_a_denied_flag")]
    #[test_case("git --git-dir=/elsewhere/.git log" => false ; "attached_value_form_is_denied")]
    #[test_case("git" => false ; "git_with_no_subcommand")]
    #[test_case("rg pattern src" => true ; "ripgrep")]
    #[test_case("rg --pre ./run.sh pattern" => false ; "ripgrep_pre_runs_a_program")]
    #[test_case("rg -z pattern" => false ; "ripgrep_search_zip")]
    #[test_case("find . -name '*.rs'" => true ; "find_by_name")]
    #[test_case("find . -delete" => false ; "find_delete")]
    #[test_case("find . -exec rm {} ;" => false ; "find_exec")]
    #[test_case("ls -la" => true ; "ls")]
    #[test_case("cat Cargo.toml" => true ; "cat")]
    #[test_case("sed -i s/a/b/ f" => false ; "sed_is_excluded_entirely")]
    #[test_case("awk {print}" => false ; "awk_is_excluded_entirely")]
    #[test_case("rm -rf build" => false ; "rm")]
    #[test_case("cargo build" => false ; "cargo_is_not_allowlisted")]
    fn single_commands_are_classified(command: &str) -> bool {
        is_read_only(&analysis(&[command]), false)
    }

    /// A line is only as safe as its worst command, and an unaccounted-for
    /// fragment makes the reviewed text describe less than the line does.
    #[test_case(&["git log", "ls"], false => true ; "every_command_observes")]
    #[test_case(&["git log", "rm -rf build"], false => false ; "one_writer_taints_the_line")]
    #[test_case(&["git log"], true => false ; "opaque_is_never_read_only")]
    #[test_case(&[], false => false ; "no_scopes_is_never_read_only")]
    fn lines_are_classified(commands: &[&str], opaque: bool) -> bool {
        is_read_only(&analysis(commands), opaque)
    }

    const PROJECT: &str = "/home/dev/project";

    #[test_case("cat notes.md" => true ; "a_relative_operand_stays_inside")]
    #[test_case("rg pattern src/deep/nested" => true ; "so_does_a_nested_one")]
    #[test_case("git log --oneline -20" => true ; "flags_are_not_paths")]
    #[test_case("git diff HEAD~1" => true ; "a_tilde_inside_a_revision_is_not_a_home_directory")]
    #[test_case("cat /etc/shadow" => false ; "an_absolute_operand_escapes")]
    #[test_case("cat ~/.ssh/id_rsa" => false ; "a_home_operand_escapes")]
    #[test_case("cat ../../secret" => false ; "a_parent_segment_escapes")]
    #[test_case("cat src/../../secret" => false ; "a_parent_segment_escapes_from_anywhere_in_the_path")]
    #[test_case("rg --file=/etc/passwd pattern" => false ; "an_attached_flag_value_escapes")]
    #[test_case("git diff --no-index /etc/passwd x" => false ; "no_index_needs_no_special_case")]
    #[test_case("cat \\/etc/shadow" => false ; "escaping_does_not_hide_an_absolute_path")]
    #[test_case("cat $HOME/.ssh/id_rsa" => false ; "a_bare_variable_is_not_a_relative_path")]
    #[test_case("cat \"$HOME\"/.ssh/id_rsa" => false ; "quoting_a_variable_does_not_stop_it_expanding")]
    #[test_case("cat `cat pointer`" => false ; "a_backtick_substitutes_too")]
    #[test_case("cat *" => false ; "an_unquoted_glob_names_what_the_text_does_not")]
    #[test_case("cat ?ecret" => false ; "so_does_a_single_character_wildcard")]
    #[test_case("cat [a-z]ecret" => false ; "and_a_bracket_class")]
    #[test_case("find . -name '*.rs'" => true ; "a_quoted_glob_is_a_literal_argument")]
    #[test_case("rg 'a.*b' src" => true ; "a_quoted_regex_is_not_a_glob")]
    fn operands_are_confined_to_the_project(command: &str) -> bool {
        stays_in_project(
            &analysis(&[command]),
            Path::new(PROJECT),
            Path::new(PROJECT),
        )
    }

    /// Caudra runs the shell unconfined, so a relative operand is only inside
    /// the project when the directory it resolves against is.
    #[test_case(PROJECT => true ; "the_project_root_itself")]
    #[test_case("/home/dev/project/crates/core" => true ; "a_directory_within_it")]
    #[test_case("/home/dev/other" => false ; "a_sibling_project")]
    #[test_case("/etc" => false ; "somewhere_else_entirely")]
    fn a_workdir_outside_the_project_confines_nothing(workdir: &str) -> bool {
        stays_in_project(
            &analysis(&["cat notes.md"]),
            Path::new(workdir),
            Path::new(PROJECT),
        )
    }

    /// A project can contain a symlink pointing anywhere, and nothing in a
    /// command's text says so. `cat notes.md` is confined by every textual rule
    /// there is and still reads whatever the link names.
    #[test_case("cat inside.md" => true ; "a real file inside the project")]
    #[test_case("cat notes.md" => false ; "a symlink to a file outside it")]
    #[test_case("cat linked/id_rsa" => false ; "a path through a symlinked directory")]
    #[test_case("cat missing.md" => true ; "a name that resolves to nothing is not a path we read")]
    #[test_case("find . -name '*.rs'" => true ; "a pattern argument still resolves to nothing")]
    fn a_symlink_out_of_the_project_is_not_confined(command: &str) -> bool {
        let project = tempfile::tempdir().expect("project");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "key").expect("secret");
        std::fs::write(project.path().join("inside.md"), "notes").expect("inside");
        std::os::unix::fs::symlink(&secret, project.path().join("notes.md")).expect("file link");
        std::os::unix::fs::symlink(outside.path(), project.path().join("linked"))
            .expect("dir link");
        let root = project.path().canonicalize().expect("canonical project");

        stays_in_project(&analysis(&[command]), &root, &root)
    }

    /// The bound is the project, not the directory the command runs from, so a
    /// link that never leaves the project stays confined even when it points
    /// outside the workdir.
    #[test]
    fn a_link_within_the_project_stays_confined_from_a_subdirectory() {
        let project = tempfile::tempdir().expect("project");
        let root = project.path().canonicalize().expect("canonical project");
        let workdir = root.join("crates");
        std::fs::create_dir(&workdir).expect("workdir");
        std::fs::write(root.join("docs.md"), "docs").expect("docs");
        std::os::unix::fs::symlink(root.join("docs.md"), workdir.join("link.md")).expect("link");

        assert!(stays_in_project(
            &analysis(&["cat link.md"]),
            &workdir,
            &root
        ));
    }

    /// Every command on the line has to stay inside, for the same reason one
    /// writer taints the line for `is_read_only`.
    #[test]
    fn one_escaping_command_taints_the_line() {
        assert!(!stays_in_project(
            &analysis(&["cat notes.md", "cat /etc/shadow"]),
            Path::new(PROJECT),
            Path::new(PROJECT),
        ));
    }
}
