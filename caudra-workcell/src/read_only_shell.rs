//! Classifies shell commands that only observe, so planning can run them.
//!
//! The allowlist is deliberately small. Anything it does not recognize is not
//! refused, it is prompted, so the list never has to be exhaustive to be safe.

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
    use super::is_read_only;
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
}
