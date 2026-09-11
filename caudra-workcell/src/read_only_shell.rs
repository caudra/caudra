//! Classifies shell commands that only observe, so planning can run them.
//!
//! The allowlist is deliberately small. Anything it does not recognize is not
//! refused, it is prompted, so the list never has to be exhaustive to be safe.
//!
//! Workcell decodes each word; this module only decides policy over the result.
//! Deciding what a word means from its raw text is what leaked `$HOME`, then a
//! brace expansion, so the decoding is not repeated here.

use std::path::{Component, Path};

use caudra_agent::permissions::{physical_boundary_check, sed_only_prints};
use workcell::shell::{ShellCommandAnalysis, ShellCommandScope, ShellWord};

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
    "rev-list",
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
const GREP: &str = "grep";
/// grep has no flag that runs a program or unpacks an archive, unlike ripgrep.
/// The one that leaves the project follows every symlink it finds rather than
/// only the ones named on the command line, which no textual check can see.
const GREP_DENIED_FLAGS: &[&str] = &["--dereference-recursive", "-R"];
const FIND: &str = "find";
/// Each of these makes find execute, delete, or write.
const FIND_DENIED_FLAGS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprintf", "-ok", "-okdir",
];
const SED: &str = "sed";
/// `awk` is absent on purpose rather than deny-listed: `system()` and `print >`
/// are reached from inside the script, so no flag list can exclude them. The
/// same is true of `sed`, which is why `sed` is recognized by its script rather
/// than by its flags and is not in the list below.
const READ_ONLY_COMMANDS: &[&str] = &[
    "basename", "cat", "date", "df", "dirname", "du", "echo", "file", "head", "jq", "ls", "printf",
    "pwd", "readlink", "realpath", "stat", "tail", "tree", "uname", "wc", "which",
];

/// Reports whether every command in an analyzed line only observes.
///
/// `opaque` means the analysis could not account for part of the line, so the
/// reviewed text describes less than the command does and nothing about it can
/// be trusted.
pub(crate) fn is_read_only(analysis: &ShellCommandAnalysis, opaque: bool) -> bool {
    !opaque && !analysis.scopes.is_empty() && analysis.scopes.iter().all(scope_is_read_only)
}

fn scope_is_read_only(scope: &ShellCommandScope) -> bool {
    // Reading every flag is the whole basis for calling `git`, `rg`, and `find`
    // observers, and a word that does not mean its own text could be any of the
    // denied ones. Plan mode gates on this answer alone, so it cannot defer the
    // question to the confinement check the way the permission path does.
    let Some(arguments) = literal_arguments(scope) else {
        return false;
    };
    match scope.executable.as_str() {
        GIT => {
            !denies(&arguments, GIT_DENIED_FLAGS)
                && arguments
                    .iter()
                    .find(|argument| !argument.starts_with('-'))
                    .is_some_and(|subcommand| GIT_READ_SUBCOMMANDS.contains(subcommand))
        }
        RG => !denies(&arguments, RG_DENIED_FLAGS),
        GREP => !denies(&arguments, GREP_DENIED_FLAGS),
        FIND => !denies(&arguments, FIND_DENIED_FLAGS),
        SED => sed_only_prints(&arguments),
        executable => READ_ONLY_COMMANDS.contains(&executable),
    }
}

/// Reports whether a line can only reach inside the project.
///
/// A read-only command still reads, and the project's own read tools are bound
/// to the project, so granting one without the same bound would make `cat` reach
/// what `file_read` has to ask about. The workdir is checked too, because Caudra
/// runs the shell unconfined and a relative operand is only inside when its base
/// is.
pub(crate) fn stays_in_project(
    analysis: &ShellCommandAnalysis,
    workdir: &Path,
    project: &Path,
) -> bool {
    workdir.starts_with(project)
        && analysis
            .scopes
            .iter()
            .all(|scope| scope_stays_in_project(scope, workdir, project))
}

fn scope_stays_in_project(scope: &ShellCommandScope, workdir: &Path, project: &Path) -> bool {
    literal_arguments(scope).is_some_and(|arguments| {
        arguments
            .iter()
            .all(|argument| argument_stays_in_project(argument, workdir, project))
    })
}

fn argument_stays_in_project(argument: &str, workdir: &Path, project: &Path) -> bool {
    // A flag carrying an attached value hides a second operand, and
    // `--file=/etc/passwd` reads it just as surely as a bare path would.
    argument
        .split('=')
        .all(|part| stays_inside(part) && resolves_inside(part, workdir, project))
}

/// The words the shell will pass, or `None` when any of them is not knowable
/// from the source and nothing can be concluded about the line.
///
/// An expansion, a glob, or a brace stands for text that is not in the command,
/// so a rule reading that text is answering about something else.
fn literal_arguments(scope: &ShellCommandScope) -> Option<Vec<&str>> {
    scope
        .arguments
        .as_ref()?
        .iter()
        .map(|word| match word {
            ShellWord::Literal(text) => Some(text.as_str()),
            ShellWord::Undecodable => None,
        })
        .collect()
}

/// Text cannot see through a symlink, and a project can contain one pointing
/// anywhere, so an operand is resolved and bounded by the same symlink-aware
/// check the file tools use.
///
/// Most words are not paths at all, and they need no special case: the rules
/// above have already excluded every form that escapes, so a flag or a pattern
/// joins the workdir and canonicalizes to its own lexical form, which is inside.
/// Only a link can leave, and only resolving finds it.
fn resolves_inside(operand: &str, workdir: &Path, project: &Path) -> bool {
    physical_boundary_check(project, &workdir.join(operand)) == Some(true)
}

/// The two forms resolution cannot be trusted to judge.
///
/// `join` treats `~` as an ordinary component, so `~/.ssh/id_rsa` would resolve
/// to a file inside the project that happens not to exist and pass. A `..`
/// climbing out of a path that does not exist has nothing to canonicalize
/// against, so it has to be read off the text.
///
/// An absolute path has neither problem and is left to resolution: `join`
/// replaces the base with it, and a path inside the project is inside the
/// project however it was spelled.
fn stays_inside(operand: &str) -> bool {
    !operand.starts_with('~')
        && !Path::new(operand)
            .components()
            .any(|component| component == Component::ParentDir)
}

fn denies(arguments: &[&str], denied: &[&str]) -> bool {
    arguments.iter().any(|argument| {
        denied.iter().any(|flag| {
            argument == flag
                || argument
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('='))
                || hides_in_cluster(argument, flag)
        })
    })
}

/// A one-letter flag can travel inside a cluster, where `rg -iz` is `rg -i -z`.
/// Matching whole words would read the cluster as a word of its own and miss it.
///
/// A cluster is only a cluster for a single-letter flag, so no long flag is
/// tested this way, and a pattern that happens to carry the letter costs a
/// prompt rather than an allowance.
fn hides_in_cluster(argument: &str, flag: &str) -> bool {
    let Some(letter) = flag.strip_prefix('-').filter(|rest| rest.len() == 1) else {
        return false;
    };
    argument
        .strip_prefix('-')
        .is_some_and(|cluster| !cluster.starts_with('-') && cluster.contains(letter))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{FIND, GIT, RG, SED, is_read_only, stays_in_project};
    use test_case::test_case;
    use workcell::shell::{ShellCommandAnalysis, ShellCommandScope, ShellWord};

    const PROJECT: &str = "/home/dev/project";

    /// Builds what Workcell reports for a line of plain words. Quoting and
    /// expansion are deliberately absent, because decoding them is Workcell's
    /// job now; the cases that need them are built from `ShellWord` directly.
    fn analysis(commands: &[&str]) -> ShellCommandAnalysis {
        ShellCommandAnalysis {
            scopes: commands
                .iter()
                .map(|command| {
                    let mut words = command.split_whitespace();
                    let executable = words.next().unwrap_or_default();
                    scope(
                        executable,
                        Some(words.map(|word| ShellWord::Literal(word.into())).collect()),
                    )
                })
                .collect(),
            opaque: false,
        }
    }

    fn scope(executable: &str, arguments: Option<Vec<ShellWord>>) -> ShellCommandScope {
        ShellCommandScope {
            start_byte: 0,
            source: executable.into(),
            normalized: executable.into(),
            permission: format!("{executable} *"),
            executable: executable.into(),
            arguments,
        }
    }

    fn one(scope: ShellCommandScope) -> ShellCommandAnalysis {
        ShellCommandAnalysis {
            scopes: vec![scope],
            opaque: false,
        }
    }

    #[test_case("git log --oneline -20" => true ; "git_log")]
    #[test_case("git status --short" => true ; "git_status")]
    #[test_case("git --no-pager diff" => true ; "a_harmless_global_flag_still_finds_the_subcommand")]
    #[test_case("git rev-list --count HEAD" => true ; "git_rev_list")]
    #[test_case("git push origin main" => false ; "git_push_is_not_a_read_subcommand")]
    #[test_case("git -c core.pager=sh log" => false ; "git_dash_c_can_run_a_pager")]
    #[test_case("git -C /elsewhere log" => false ; "git_dash_big_c_leaves_the_repository")]
    #[test_case("git --git-dir=/elsewhere/.git log" => false ; "attached_value_form_is_denied")]
    #[test_case("git" => false ; "git_with_no_subcommand")]
    #[test_case("rg pattern src" => true ; "ripgrep")]
    #[test_case("rg --pre ./run.sh pattern" => false ; "ripgrep_pre_runs_a_program")]
    #[test_case("rg -z pattern" => false ; "ripgrep_search_zip")]
    #[test_case("rg -iz pattern" => false ; "ripgrep_search_zip_inside_a_cluster")]
    #[test_case("git --no-pager diff" => true ; "a_long_flag_is_not_a_cluster")]
    #[test_case("grep -rn needle src" => true ; "grep_recursively")]
    #[test_case("grep -R needle src" => false ; "grep_dereferencing_every_symlink")]
    #[test_case("grep -Rn needle src" => false ; "grep_dereferencing_from_inside_a_cluster")]
    #[test_case("find . -name *.rs" => true ; "find_by_name")]
    #[test_case("find . -delete" => false ; "find_delete")]
    #[test_case("find . -exec rm x ;" => false ; "find_exec")]
    #[test_case("ls -la" => true ; "ls")]
    #[test_case("cat Cargo.toml" => true ; "cat")]
    #[test_case("echo === callers ===" => true ; "echo")]
    #[test_case("printf %s-%s a b" => true ; "printf")]
    #[test_case("sed -n 1,140p f" => true ; "sed_printing_a_slice")]
    #[test_case("sed -i s/a/b/ f" => false ; "sed_in_place_writes")]
    #[test_case("sed -n 1w/tmp/x f" => false ; "sed_writing_from_inside_the_script")]
    #[test_case("awk -f script.awk" => false ; "awk_is_excluded_entirely")]
    #[test_case("rm -rf build" => false ; "rm")]
    #[test_case("cargo build" => false ; "cargo_is_not_allowlisted")]
    fn single_commands_are_classified(command: &str) -> bool {
        is_read_only(&analysis(&[command]), false)
    }

    /// A line is only as safe as its worst command, and an unaccounted-for
    /// fragment makes the reviewed text describe less than the line does.
    #[test_case(&["git log", "ls"], false => true ; "every_command_observes")]
    #[test_case(&["sed -n 1,10p f", "echo ===", "grep -n y f"], false => true ; "a_read_wrapped_in_glue")]
    #[test_case(&["git log", "rm -rf build"], false => false ; "one_writer_taints_the_line")]
    #[test_case(&["git log"], true => false ; "opaque_is_never_read_only")]
    #[test_case(&[], false => false ; "no_scopes_is_never_read_only")]
    fn lines_are_classified(commands: &[&str], opaque: bool) -> bool {
        is_read_only(&analysis(commands), opaque)
    }

    /// Reading every flag is what makes these commands observers, so a word that
    /// does not mean its own text could be any of the denied ones. Plan mode
    /// gates on `is_read_only` alone, so refusing here is the only thing that
    /// stops `find . $FLAG` running while planning.
    #[test_case(GIT => false ; "git_could_be_hiding_dash_c")]
    #[test_case(RG => false ; "ripgrep_could_be_hiding_pre")]
    #[test_case(FIND => false ; "find_could_be_hiding_exec")]
    #[test_case(SED => false ; "sed_could_be_hiding_its_script")]
    #[test_case("cat" => false ; "and_a_plain_reader_would_read_something_unnamed")]
    fn an_undecodable_word_is_never_read_only(executable: &str) -> bool {
        is_read_only(
            &one(scope(executable, Some(vec![ShellWord::Undecodable]))),
            false,
        )
    }

    /// Words that were never enumerated are not words that turned out to be
    /// absent. Treating the first as the second calls an unexamined line safe.
    #[test_case(None => false ; "a_scope_whose_words_were_never_read")]
    #[test_case(Some(Vec::new()) => true ; "a_command_that_really_takes_none")]
    fn unenumerated_words_are_not_absent_words(arguments: Option<Vec<ShellWord>>) -> bool {
        is_read_only(&one(scope("pwd", arguments)), false)
    }

    #[test_case("cat notes.md" => true ; "a_relative_operand_stays_inside")]
    #[test_case("rg pattern src/deep/nested" => true ; "so_does_a_nested_one")]
    #[test_case("git log --oneline -20" => true ; "flags_are_not_paths")]
    #[test_case("git diff HEAD~1" => true ; "a_tilde_inside_a_revision_is_not_a_home_directory")]
    #[test_case("cat /home/dev/project/notes.md" => true ; "an_absolute_operand_inside_is_inside")]
    #[test_case("cat /etc/shadow" => false ; "an_absolute_operand_escapes")]
    #[test_case("cat ~/.ssh/id_rsa" => false ; "a_home_operand_escapes")]
    #[test_case("cat ../../secret" => false ; "a_parent_segment_escapes")]
    #[test_case("cat src/../../secret" => false ; "a_parent_segment_escapes_from_anywhere_in_the_path")]
    #[test_case("rg --file=/etc/passwd pattern" => false ; "an_attached_flag_value_escapes")]
    #[test_case("git diff --no-index /etc/passwd x" => false ; "no_index_needs_no_special_case")]
    #[test_case("find . -name *.rs" => true ; "a_decoded_glob_is_an_ordinary_argument")]
    #[test_case("rg a.*b src" => true ; "a_decoded_regex_is_not_a_path")]
    fn operands_are_confined_to_the_project(command: &str) -> bool {
        stays_in_project(
            &analysis(&[command]),
            Path::new(PROJECT),
            Path::new(PROJECT),
        )
    }

    /// An undecodable word stands for text that is not in the line, so where it
    /// points cannot be read off it either.
    #[test_case(Some(vec![ShellWord::Undecodable]) => false ; "a_word_that_does_not_mean_its_own_text")]
    #[test_case(None => false ; "words_that_were_never_read")]
    fn unreadable_words_are_never_confined(arguments: Option<Vec<ShellWord>>) -> bool {
        stays_in_project(
            &one(scope("cat", arguments)),
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
    #[test_case("find . -name *.rs" => true ; "a pattern argument still resolves to nothing")]
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
