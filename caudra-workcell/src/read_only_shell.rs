//! Classifies shell commands that only observe, so planning can run them.
//!
//! The allowlist is deliberately small. Anything it does not recognize is not
//! refused, it is prompted, so the list never has to be exhaustive to be safe.
//!
//! Workcell decodes each word; this module only decides policy over the result.
//! Deciding what a word means from its raw text is what leaked `$HOME`, then a
//! brace expansion, so the decoding is not repeated here.

use std::path::{Component, Path, PathBuf};

use caudra_agent::permissions::{
    PermissionResourceAccess, PermissionResourceKind, filesystem_permission_resource,
    physical_boundary_check, sed_only_prints,
};
#[cfg(test)]
use workcell::shell::ShellCommandAnalysis;
use workcell::shell::{ShellCommandScope, ShellWord, bash::BashCwdSet};

const GIT: &str = "git";
const GIT_READ_SUBCOMMANDS: &[&str] = &[
    "blame",
    "describe",
    "diff",
    "log",
    "ls-files",
    "ls-tree",
    "rev-list",
    "rev-parse",
    "shortlog",
    "show",
    "status",
];
const GIT_READ_FLAGS: &[&str] = &[
    "--oneline",
    "--short",
    "--stat",
    "--numstat",
    "--name-only",
    "--name-status",
    "--summary",
    "--check",
    "--cached",
    "--staged",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--color=never",
    "--no-renames",
    "--no-index",
    "--raw",
    "--patch",
    "--quiet",
    "--exit-code",
    "--all",
    "--count",
    "--abbrev-ref",
    "--show-toplevel",
    "--show-prefix",
    "--verify",
    "--porcelain",
    "--porcelain=v1",
    "--porcelain=v2",
    "--graph",
    "--decorate",
    "--no-decorate",
    "--reverse",
    "--first-parent",
    "--no-merges",
    "--merges",
    "--left-right",
    "--cherry-pick",
    "--boundary",
    "--tags",
    "--heads",
    "--remotes",
    "--full-name",
    "--full-tree",
    "--long",
    "-p",
    "-s",
    "-u",
    "-uno",
    "-z",
    "-r",
    "-t",
    "-v",
];
const GIT_LIST_FLAGS: &[&str] = &["--list", "-l", "--no-color", "--color=never"];
const GIT_BRANCH_FLAGS: &[&str] = &["--all", "-a", "--remotes", "-r", "--verbose", "-v", "-vv"];
const RG: &str = "rg";
const RG_DENIED_FLAGS: &[&str] = &[
    "--hostname-bin",
    "--pre",
    "--pre-glob",
    "--search-zip",
    "--follow",
    "-L",
    "-z",
];
const GREP: &str = "grep";
/// grep has no flag that runs a program or unpacks an archive, unlike ripgrep.
/// The one that leaves the project follows every symlink it finds rather than
/// only the ones named on the command line, which no textual check can see.
const GREP_DENIED_FLAGS: &[&str] = &["--dereference-recursive", "-R"];
const FIND: &str = "find";
const FIND_DENIED_FLAGS: &[&str] = &[
    "-delete",
    "-exec",
    "-execdir",
    "-fls",
    "-fprint",
    "-fprint0",
    "-fprintf",
    "-ok",
    "-okdir",
    "-L",
    "-H",
    "-files0-from",
];
const SORT: &str = "sort";
const SORT_READ_FLAGS: &[&str] = &[
    "--unique",
    "--reverse",
    "--numeric-sort",
    "--human-numeric-sort",
    "--general-numeric-sort",
    "--ignore-case",
    "--ignore-leading-blanks",
    "--dictionary-order",
    "--ignore-nonprinting",
    "--month-sort",
    "--stable",
    "--version-sort",
    "--check",
    "--zero-terminated",
];
const SORT_READ_SHORT_FLAGS: &str = "urnhgfbdiMsVcCz";
const SED: &str = "sed";
const CD: &str = "cd";
/// `awk` is absent on purpose rather than deny-listed: `system()` and `print >`
/// are reached from inside the script, so no flag list can exclude them. The
/// same is true of `sed`, which is why `sed` is recognized by its script rather
/// than by its flags and is not in the list below.
const READ_ONLY_COMMANDS: &[&str] = &[
    "basename", "cat", "cd", "df", "dirname", "echo", "head", "ls", "ps", "pwd", "readlink",
    "realpath", "stat", "tail", "uname", "which",
];
const INDIRECT_FILE_FLAGS: &[&str] = &["--files0-from"];
const FILE_READ_FLAGS: &[&str] = &[
    "-b",
    "--brief",
    "-i",
    "--mime",
    "--mime-type",
    "--mime-encoding",
    "-h",
    "--no-dereference",
];
const TREE_READ_FLAGS: &[&str] = &[
    "-a",
    "-d",
    "-f",
    "-i",
    "-p",
    "-s",
    "--dirsfirst",
    "--noreport",
];

/// Reports whether every command in an analyzed line only observes.
///
/// `opaque` means the analysis could not account for part of the line, so the
/// reviewed text describes less than the command does and nothing about it can
/// be trusted.
#[cfg(test)]
fn is_read_only(analysis: &ShellCommandAnalysis, opaque: bool) -> bool {
    !opaque && !analysis.scopes.is_empty() && analysis.scopes.iter().all(scope_is_read_only)
}

pub(crate) fn scope_is_read_only(scope: &ShellCommandScope) -> bool {
    if scope.source != scope.normalized {
        return false;
    }
    // Reading every flag is the whole basis for calling `git`, `rg`, and `find`
    // observers, and a word that does not mean its own text could be any of the
    // denied ones. Plan mode gates on this answer alone, so it cannot defer the
    // question to the confinement check the way the permission path does.
    let Some(arguments) = literal_arguments(scope) else {
        return false;
    };
    match scope.executable.as_str() {
        GIT => git_is_read_only(&arguments),
        RG => !denies(&arguments, RG_DENIED_FLAGS) && no_attached_pattern_file(&arguments),
        GREP => !denies(&arguments, GREP_DENIED_FLAGS) && no_attached_pattern_file(&arguments),
        FIND => !denies(&arguments, FIND_DENIED_FLAGS),
        SORT => arguments.iter().all(|argument| {
            !argument.starts_with('-')
                || SORT_READ_FLAGS.contains(argument)
                || argument.strip_prefix('-').is_some_and(|flags| {
                    !flags.is_empty()
                        && flags
                            .chars()
                            .all(|flag| SORT_READ_SHORT_FLAGS.contains(flag))
                })
        }),
        SED => sed_only_prints(&arguments),
        "date" => arguments.iter().all(|argument| {
            matches!(*argument, "-u" | "--utc" | "--universal") || argument.starts_with('+')
        }),
        "file" => only_flags_and_operands(&arguments, FILE_READ_FLAGS),
        "tree" => only_flags_and_operands(&arguments, TREE_READ_FLAGS),
        "du" | "wc" => !denies(&arguments, INDIRECT_FILE_FLAGS),
        "printf" => arguments
            .first()
            .is_some_and(|format| !format.starts_with('-') || *format == "--"),
        executable => READ_ONLY_COMMANDS.contains(&executable),
    }
}

fn git_is_read_only(mut arguments: &[&str]) -> bool {
    while let [
        "--no-pager" | "--literal-pathspecs" | "--no-optional-locks",
        rest @ ..,
    ] = arguments
    {
        arguments = rest;
    }
    let Some((&subcommand, arguments)) = arguments.split_first() else {
        return false;
    };
    match subcommand {
        "branch" if arguments == ["--show-current"] => true,
        "branch" | "tag" => {
            let mut listing = false;
            arguments.iter().all(|argument| {
                if matches!(*argument, "--list" | "-l") {
                    listing = true;
                }
                GIT_LIST_FLAGS.contains(argument)
                    || (subcommand == "branch" && GIT_BRANCH_FLAGS.contains(argument))
                    || (listing && !argument.starts_with('-'))
            })
        }
        "reflog" => match arguments {
            [] => true,
            ["show", rest @ ..] => git_read_arguments(rest),
            _ => false,
        },
        subcommand => GIT_READ_SUBCOMMANDS.contains(&subcommand) && git_read_arguments(arguments),
    }
}

fn git_read_arguments(arguments: &[&str]) -> bool {
    let mut arguments = arguments.iter().copied();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            return true;
        }
        if matches!(argument, "-n" | "--max-count") {
            if !arguments.next().is_some_and(decimal) {
                return false;
            }
        } else if argument.starts_with('-')
            && !GIT_READ_FLAGS.contains(&argument)
            && !argument.strip_prefix('-').is_some_and(decimal)
            && !argument.strip_prefix("--max-count=").is_some_and(decimal)
        {
            return false;
        }
    }
    true
}

fn decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn only_flags_and_operands(arguments: &[&str], flags: &[&str]) -> bool {
    arguments
        .iter()
        .all(|argument| !argument.starts_with('-') || flags.contains(argument))
}

fn no_attached_pattern_file(arguments: &[&str]) -> bool {
    arguments
        .iter()
        .all(|argument| *argument == "-f" || !hides_in_cluster(argument, "-f"))
}

pub(crate) fn confined_read(
    scope: &ShellCommandScope,
    incoming: &BashCwdSet,
    project: &Path,
) -> bool {
    let BashCwdSet::Known(directories) = incoming else {
        return false;
    };
    !directories.is_empty()
        && scope_is_read_only(scope)
        && directories.iter().all(|directory| {
            directory.starts_with(project)
                && resolves_inside("", directory, project)
                && if scope.executable == CD {
                    cd_target(scope, directory, project).is_some()
                } else {
                    scope_stays_in_project(scope, directory, project)
                }
        })
}

/// Where a `cd` leaves the shell, or `None` when the command does not say, or
/// says somewhere outside the project.
fn cd_target(scope: &ShellCommandScope, current: &Path, project: &Path) -> Option<PathBuf> {
    if !scope_is_read_only(scope) {
        return None;
    }
    // No operand is `$HOME`, and two is the substitution form `cd old new`.
    let arguments = literal_arguments(scope)?;
    let target = match arguments.as_slice() {
        [target] | ["--", target] => *target,
        _ => return None,
    };
    if target.starts_with('-') || !stays_inside(target) {
        return None;
    }
    let moved = current.join(target);
    resolves_inside(target, current, project).then_some(moved)
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
        .split(['=', ':'])
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
    let path = workdir.join(operand);
    physical_boundary_check(project, &path) == Some(true)
        && !filesystem_permission_resource(
            PermissionResourceKind::File,
            &path,
            PermissionResourceAccess::Read,
            project,
        )
        .requires_prompt
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
                || (argument.starts_with("--")
                    && argument.len() > 2
                    && flag.starts_with(argument.split('=').next().unwrap_or(argument)))
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

    use super::{FIND, GIT, RG, SED, confined_read, is_read_only};
    use test_case::test_case;
    use workcell::shell::bash::{BashContextAssumptions, parse_bash};
    use workcell::shell::{ShellCommandAnalysis, ShellCommandScope, ShellWord};

    const PROJECT: &str = "/home/dev/project";
    const COMMAND_SEPARATOR: &str = " && ";

    fn confined_reads(
        analysis: &ShellCommandAnalysis,
        workdir: &Path,
        project: &Path,
    ) -> Vec<bool> {
        let source = analysis
            .scopes
            .iter()
            .map(|scope| scope.source.as_str())
            .collect::<Vec<_>>()
            .join(COMMAND_SEPARATOR);
        let program = parse_bash(&source).expect("program");
        let contexts = program.command_contexts_with_assumptions(
            workdir,
            BashContextAssumptions {
                startup_preserves_cwd: true,
                no_aliases_functions_or_command_not_found_hook: true,
                no_traps: true,
                default_shell_options: true,
                standard_builtins: true,
                directory_variables_are_standard: true,
                cdpath_empty: true,
                lastpipe_disabled: true,
                logical_pwd_matches_initial: true,
            },
        );
        analysis
            .scopes
            .iter()
            .map(|scope| {
                contexts
                    .commands
                    .iter()
                    .find(|context| {
                        program.nodes()[context.command.0].span.start == scope.start_byte
                    })
                    .is_some_and(|context| confined_read(scope, &context.incoming, project))
            })
            .collect()
    }

    /// Whether a whole line is confined, for the cases about how one command's
    /// operands resolve rather than about which command the answer lands on.
    fn line_is_confined(analysis: &ShellCommandAnalysis, workdir: &Path, project: &Path) -> bool {
        let confined = confined_reads(analysis, workdir, project);
        !confined.is_empty() && confined.iter().all(|command| *command)
    }

    /// Builds what Workcell reports for a line of plain words. Quoting and
    /// expansion are deliberately absent, because decoding them is Workcell's
    /// job now; the cases that need them are built from `ShellWord` directly.
    fn analysis(commands: &[&str]) -> ShellCommandAnalysis {
        let mut offset = 0;
        ShellCommandAnalysis {
            scopes: commands
                .iter()
                .map(|command| {
                    let mut words = command.split_whitespace();
                    let executable = words.next().unwrap_or_default();
                    let mut scope = scope(
                        executable,
                        Some(words.map(|word| ShellWord::Literal(word.into())).collect()),
                    );
                    scope.source = (*command).into();
                    scope.normalized = (*command).into();
                    scope.start_byte = offset;
                    offset += command.len() + COMMAND_SEPARATOR.len();
                    scope
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
    #[test_case("cd crates" => true ; "cd_moves_nothing_on_disk")]
    #[test_case("ps aux" => true ; "ps")]
    #[test_case("sort -u f" => true ; "sort_to_stdout")]
    #[test_case("sort -o out f" => false ; "sort_writing_to_a_file")]
    #[test_case("sort -uo out f" => false ; "sort_writing_from_inside_a_cluster")]
    #[test_case("sort --compress-program gzip f" => false ; "sort_running_a_program")]
    #[test_case("sort -T /etc f" => false ; "sort_writing_temporaries_elsewhere")]
    #[test_case("echo === callers ===" => true ; "echo")]
    #[test_case("printf %s-%s a b" => true ; "printf")]
    #[test_case("sed -n 1,140p f" => true ; "sed_printing_a_slice")]
    #[test_case("sed -i s/a/b/ f" => false ; "sed_in_place_writes")]
    #[test_case("sed -n 1w/tmp/x f" => false ; "sed_writing_from_inside_the_script")]
    #[test_case("awk -f script.awk" => false ; "awk_is_excluded_entirely")]
    #[test_case("rm -rf build" => false ; "rm")]
    #[test_case("cargo build" => false ; "cargo_is_not_allowlisted")]
    #[test_case("git branch -D topic" => false ; "branch_force_delete")]
    #[test_case("git branch --list -D topic" => false ; "branch_list_with_delete")]
    #[test_case("git branch topic" => false ; "branch_create")]
    #[test_case("git tag release" => false ; "tag_create")]
    #[test_case("git reflog expire --all" => false ; "reflog_expire")]
    #[test_case("git reflog delete HEAD" => false ; "reflog_delete")]
    #[test_case("git branch -a -vv" => true ; "branch_list")]
    #[test_case("git tag --list v1" => true ; "tag_list")]
    #[test_case("git reflog show -3" => true ; "reflog_show")]
    #[test_case("git diff --out=output" => false ; "git_abbreviated_output")]
    #[test_case("git show --textconv HEAD" => false ; "git_textconv_helper")]
    #[test_case("date --se=now" => false ; "date_abbreviated_set")]
    #[test_case("file -C -m magic" => false ; "file_compile")]
    #[test_case("tree -o output" => false ; "tree_output")]
    #[test_case("printf -v PATH value" => false ; "printf_assignment")]
    #[test_case("sort --out=output input" => false ; "sort_abbreviated_output")]
    #[test_case("grep --dereference-r needle ." => false ; "grep_abbreviated_follow")]
    #[test_case("find . -fprint0 output" => false ; "find_null_output")]
    fn single_commands_are_classified(command: &str) -> bool {
        is_read_only(&analysis(&[command]), false)
    }

    #[test_case("./cat", "cat"; "relative_reader")]
    #[test_case("/usr/bin/cat", "cat"; "absolute_reader")]
    #[test_case("'cat'", "cat"; "quoted_reader")]
    #[test_case("./cd", "cd"; "relative_directory_change")]
    fn normalized_executables_do_not_prove_read_authority(source: &str, executable: &str) {
        let mut command = scope(executable, Some(vec![ShellWord::Literal(".".into())]));
        command.source = format!("{source} .");
        command.normalized = format!("{executable} .");
        let analysis = one(command);

        assert!(!is_read_only(&analysis, false));
        assert_eq!(
            confined_reads(&analysis, Path::new(PROJECT), Path::new(PROJECT)),
            vec![false]
        );
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
    #[test_case("cat .env" => false ; "protected_dotenv")]
    #[test_case("cat .git/config" => false ; "protected_git_config")]
    #[test_case("cat .git/HEAD" => true ; "inert_git_metadata")]
    #[test_case("git show HEAD:.env" => false ; "protected_revision_path")]
    #[test_case("rg --file=.env needle" => false ; "protected_attached_operand")]
    fn operands_are_confined_to_the_project(command: &str) -> bool {
        line_is_confined(
            &analysis(&[command]),
            Path::new(PROJECT),
            Path::new(PROJECT),
        )
    }

    /// Models restate the directory they are already in and then read, 1,422
    /// times in this machine's history. Every later operand is judged against
    /// the directory the `cd` reached, so following it is what makes the line
    /// answerable at all.
    #[test_case(&["cd crates", "cat core/lib.rs"] => true ; "a_relative_move_within_the_project")]
    #[test_case(&["cd /home/dev/project/crates", "cat lib.rs"] => true ; "the_absolute_form_models_send")]
    #[test_case(&["cd /home/dev/project", "rg needle src"] => true ; "the_project_root_restated")]
    #[test_case(&["cat a", "cd crates", "cat b"] => true ; "a_read_on_either_side_of_the_move")]
    #[test_case(&["cd crates", "cat ../../secret"] => false ; "a_climb_out_of_the_directory_it_reached")]
    #[test_case(&["cd /tmp", "cat x"] => false ; "a_move_out_of_the_project")]
    #[test_case(&["cd", "cat x"] => false ; "no_operand_is_the_home_directory")]
    #[test_case(&["cd -", "cat x"] => false ; "a_dash_is_the_previous_directory")]
    #[test_case(&["cd ~", "cat x"] => false ; "a_tilde_is_the_home_directory")]
    #[test_case(&["cd ..", "cat x"] => false ; "a_parent_segment_leaves_the_project")]
    #[test_case(&["cd a b", "cat x"] => false ; "two_operands_are_the_substitution_form")]
    #[test_case(&["rg needle src", "cd /tmp"] => false ; "a_trailing_move_counts_too")]
    #[test_case(&["cd --", "cat x"] => false ; "an_option_is_not_a_directory")]
    #[test_case(&["cd .git", "cat config"] => false ; "protected_working_directory")]
    fn a_directory_change_is_followed(commands: &[&str]) -> bool {
        line_is_confined(&analysis(commands), Path::new(PROJECT), Path::new(PROJECT))
    }

    /// An undecodable word stands for text that is not in the line, so where it
    /// points cannot be read off it either.
    #[test_case(Some(vec![ShellWord::Undecodable]) => false ; "a_word_that_does_not_mean_its_own_text")]
    #[test_case(None => false ; "words_that_were_never_read")]
    fn unreadable_words_are_never_confined(arguments: Option<Vec<ShellWord>>) -> bool {
        line_is_confined(
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
        line_is_confined(
            &analysis(&["cat notes.md"]),
            Path::new(workdir),
            Path::new(PROJECT),
        )
    }

    /// A command with no operands gives the operand check nothing to judge, so
    /// the workdir is the only thing saying where it reads. Removing the workdir
    /// bound left every other case passing, because an operand outside the
    /// project fails on its own resolution.
    #[test_case("ls" => false ; "a_listing_of_wherever_it_runs")]
    #[test_case("pwd" => false ; "a_command_naming_the_directory_it_runs_in")]
    fn a_reader_with_no_operands_is_bound_by_the_workdir(command: &str) -> bool {
        line_is_confined(&analysis(&[command]), Path::new("/etc"), Path::new(PROJECT))
    }

    /// A project can contain a symlink pointing anywhere, and nothing in a
    /// command's text says so. `cat notes.md` is confined by every textual rule
    /// there is and still reads whatever the link names.
    #[test_case(&["cat inside.md"] => true ; "a real file inside the project")]
    #[test_case(&["cat notes.md"] => false ; "a symlink to a file outside it")]
    #[test_case(&["cat linked/id_rsa"] => false ; "a path through a symlinked directory")]
    #[test_case(&["cat missing.md"] => true ; "a name that resolves to nothing is not a path we read")]
    #[test_case(&["find . -name *.rs"] => true ; "a pattern argument still resolves to nothing")]
    #[test_case(&["cd linked"] => false ; "a move through a symlinked directory")]
    // `sub/escape` leaves the project and `escape` names nothing, so the answer
    // is only right when the operand is resolved against the directory the `cd`
    // reached. Against the workdir it resolves to nothing and reads as confined.
    #[test_case(&["cd sub", "cat escape"] => false ; "a link the move brings into reach")]
    fn a_symlink_out_of_the_project_is_not_confined(commands: &[&str]) -> bool {
        let project = tempfile::tempdir().expect("project");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "key").expect("secret");
        std::fs::write(project.path().join("inside.md"), "notes").expect("inside");
        std::os::unix::fs::symlink(&secret, project.path().join("notes.md")).expect("file link");
        std::os::unix::fs::symlink(outside.path(), project.path().join("linked"))
            .expect("dir link");
        std::fs::create_dir(project.path().join("sub")).expect("subdirectory");
        std::os::unix::fs::symlink(&secret, project.path().join("sub/escape"))
            .expect("nested link");
        let root = project.path().canonicalize().expect("canonical project");

        line_is_confined(&analysis(commands), &root, &root)
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

        assert!(line_is_confined(
            &analysis(&["cat link.md"]),
            &workdir,
            &root
        ));
    }

    /// Each command is authorized on its own, so one that escapes or writes
    /// costs its own prompt and no longer taints the line. Answering for the
    /// line as a whole is what made a `cd` into the directory the shell already
    /// sat in ask alongside the `cargo` it preceded.
    ///
    /// A `cd` is the one command whose answer reaches the others, because it
    /// moves what their relative operands resolve against. One that cannot be
    /// placed leaves the rest unanswerable rather than merely unconfined, so it
    /// disqualifies them too.
    #[test_case(&["cat notes.md", "cat /etc/shadow"] => vec![true, false] ; "a_read_beside_one_that_escapes")]
    #[test_case(&["cat notes.md", "rm -rf build"] => vec![true, false] ; "a_read_beside_a_writer")]
    #[test_case(&["cargo check", "rg needle src"] => vec![false, true] ; "a_read_after_something_that_executes")]
    #[test_case(&["cd crates", "cargo build", "cat lib.rs"] => vec![true, false, true] ; "the_reached_directory_survives_a_command_that_executes")]
    #[test_case(&["cd /tmp", "cat x"] => vec![false, false] ; "a_move_out_of_the_project_disqualifies_the_rest")]
    #[test_case(&["cd -", "cat notes.md"] => vec![false, false] ; "a_move_nothing_can_place_disqualifies_the_rest")]
    fn each_command_on_the_line_is_answered_on_its_own(commands: &[&str]) -> Vec<bool> {
        confined_reads(&analysis(commands), Path::new(PROJECT), Path::new(PROJECT))
    }
}
