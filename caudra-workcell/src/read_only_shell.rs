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
    PermissionResourceAccess, PermissionResourceKind, executables::versioned_interpreter,
    filesystem_permission_resource, physical_boundary_check, sed_only_prints,
};
#[cfg(test)]
use workcell::shell::ShellCommandAnalysis;
use workcell::shell::{ShellCommandScope, ShellWord, bash::BashCwdSet};

use crate::shell_glob::expand_glob;

const VERSION_FLAG: &str = "--version";
const VERSION_ONLY: &[&str] = &[VERSION_FLAG];
/// Executables whose version flags only print, keyed by the name
/// `versioned_interpreter` resolves to. A probe is one of these flags and
/// nothing else. `python -v` starts a verbose REPL and `ruby -v` reads a
/// program from stdin, so neither flag is listed.
const VERSION_PROBES: &[(&str, &[&str])] = &[
    ("python", &[VERSION_FLAG, "-V"]),
    ("node", &[VERSION_FLAG, "-v"]),
    ("npm", &[VERSION_FLAG, "-v"]),
    ("pnpm", &[VERSION_FLAG, "-v"]),
    ("yarn", &[VERSION_FLAG, "-v"]),
    ("deno", VERSION_ONLY),
    ("bun", VERSION_ONLY),
    ("cargo", &[VERSION_FLAG, "-V"]),
    ("rustc", &[VERSION_FLAG, "-V"]),
    ("rustup", &[VERSION_FLAG, "-V"]),
    ("go", &["version"]),
    ("java", &["-version", VERSION_FLAG]),
    ("ruby", VERSION_ONLY),
    ("perl", &[VERSION_FLAG, "-v"]),
    ("gcc", VERSION_ONLY),
    ("clang", VERSION_ONLY),
    ("make", VERSION_ONLY),
    ("cmake", VERSION_ONLY),
    (GIT, VERSION_ONLY),
    ("just", VERSION_ONLY),
    (RG, VERSION_ONLY),
    ("jq", VERSION_ONLY),
    ("uv", VERSION_ONLY),
    ("pip", VERSION_ONLY),
    ("pip3", VERSION_ONLY),
    ("nix", VERSION_ONLY),
    ("docker", VERSION_ONLY),
];
const LOOKUP_COMMAND: &str = "command";
const LOOKUP_TYPE: &str = "type";
const LOOKUP_FLAGS: &[&str] = &["-v", "-V"];
const TYPE_FLAGS: &str = "afptP";
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
    "--pretty",
    "--abbrev-commit",
    "--no-abbrev-commit",
    "--follow",
    "--shortstat",
    "--no-patch",
    "-p",
    "-s",
    "-u",
    "-uno",
    "-z",
    "-r",
    "-t",
    "-v",
];
/// Flags read only in their attached form, so the value can never be taken for
/// the next operand or the next operand for the value.
const GIT_READ_VALUE_FLAGS: &[&str] = &[
    "--format=",
    "--pretty=",
    "--date=",
    "--abbrev=",
    "--since=",
    "--until=",
    "--after=",
    "--before=",
    "--author=",
    "--committer=",
    "--grep=",
    "--skip=",
];
const GIT_FORMAT_FLAGS: &[&str] = &["--format=", "--pretty="];
/// `%G?`, `%GS`, and the rest of the signature placeholders verify a signature,
/// which runs gpg.
const GIT_SIGNATURE_PLACEHOLDER: &str = "%G";
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
/// grep's flag set is small and settled, so it is read as an allow-list: a flag
/// a future release adds costs a prompt instead of quietly widening what a
/// read-only agent may run. `--file` and `--exclude-from` read a path nobody
/// reviewed, and `-R` follows every symlink it meets rather than only the ones
/// named on the command line, which no textual check can see. All three are
/// absent below rather than denied.
const GREP_READ_FLAGS: &[&str] = &[
    "--after-context",
    "--basic-regexp",
    "--before-context",
    "--binary",
    "--binary-files",
    "--byte-offset",
    "--color",
    "--colour",
    "--context",
    "--count",
    "--exclude",
    "--exclude-dir",
    "--extended-regexp",
    "--files-with-matches",
    "--files-without-match",
    "--fixed-strings",
    "--group-separator",
    "--ignore-case",
    "--include",
    "--initial-tab",
    "--invert-match",
    "--label",
    "--line-buffered",
    "--line-number",
    "--line-regexp",
    "--max-count",
    "--no-filename",
    "--no-group-separator",
    "--no-ignore-case",
    "--no-messages",
    "--null",
    "--null-data",
    "--only-matching",
    "--perl-regexp",
    "--quiet",
    "--recursive",
    "--regexp",
    "--silent",
    "--text",
    "--with-filename",
    "--word-regexp",
];
const GREP_READ_SHORT_FLAGS: &str = "ABCEFGHIPTUabcehilmnoqrsvwxyzZ";
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
const END_OF_FLAGS: &str = "--";
const CD: &str = "cd";
/// `awk` is absent on purpose rather than deny-listed: `system()` and `print >`
/// are reached from inside the script, so no flag list can exclude them. The
/// same is true of `sed`, which is why `sed` is recognized by its script rather
/// than by its flags and is not in the list below.
const READ_ONLY_COMMANDS: &[&str] = &[
    "basename", "cat", "cd", "df", "dirname", "echo", "head", "ls", "ps", "pwd", "readlink",
    "realpath", "stat", "tail", "uname", "which",
];
/// Readers that take every operand as a path to read, so a safe glob among
/// them only names more of the same. `echo` and the name tools print or
/// resolve what they are given instead.
const GLOB_READERS: &[&str] = &[
    "cat", "du", "file", "head", "ls", SORT, "stat", "tail", "tree", "wc",
];
/// `du` and `wc` write nothing, so the allow-list is about what they read:
/// `--files0-from` takes its operands from a file nobody reviewed, and the
/// dereference flags walk out of the project through a symlink. Both are absent
/// rather than denied, so a new flag has to be added here to be allowed.
const DU_READ_FLAGS: &[&str] = &[
    "--all",
    "--apparent-size",
    "--block-size",
    "--bytes",
    "--count-links",
    "--exclude",
    "--human-readable",
    "--inodes",
    "--max-depth",
    "--null",
    "--one-file-system",
    "--separate-dirs",
    "--si",
    "--summarize",
    "--threshold",
    "--time",
    "--time-style",
    "--total",
];
const DU_READ_SHORT_FLAGS: &str = "0BPabcdhklmstx";
const WC_READ_FLAGS: &[&str] = &[
    "--bytes",
    "--chars",
    "--lines",
    "--max-line-length",
    "--total",
    "--words",
];
const WC_READ_SHORT_FLAGS: &str = "Lclmw";
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
    !opaque
        && !analysis.scopes.is_empty()
        && analysis
            .scopes
            .iter()
            .all(|scope| scope_is_read_only(scope, &[]))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NotReadOnly {
    #[error("command source differs from its normalized scope")]
    SourceMismatch,
    #[error("command arguments are not fully decoded")]
    UndecodableWord,
    #[error("executable has no deterministic read-only rule")]
    UnknownCommand,
    #[error("invocation is not supported by the executable's read-only rule")]
    UnsupportedInvocation,
}

pub(crate) fn scope_is_read_only(scope: &ShellCommandScope, globs: &[Option<&str>]) -> bool {
    shell_read_only_verdict(scope, globs).is_ok()
}

/// Classifies only the decoded command scope, not line effects or confinement.
/// A failure means read-only is unproven, not that the command writes.
///
/// `globs` holds each argument's pattern when it is a safe glob. One reads as
/// its own text where its matches can only be paths to read, which never
/// start with a dash, and confinement expands it.
pub(crate) fn shell_read_only_verdict(
    scope: &ShellCommandScope,
    globs: &[Option<&str>],
) -> Result<(), NotReadOnly> {
    if scope.source != scope.normalized {
        return Err(NotReadOnly::SourceMismatch);
    }
    // Reading every flag is the whole basis for calling `git`, `rg`, and `find`
    // observers, and a word that does not mean its own text could be any of the
    // denied ones. Plan mode gates on this answer alone, so it cannot defer the
    // question to the confinement check the way the permission path does.
    let arguments = reviewed_arguments(scope, globs)
        .filter(|arguments| globs_name_read_paths(&scope.executable, arguments, globs))
        .ok_or(NotReadOnly::UndecodableWord)?;
    let read_only = match scope.executable.as_str() {
        executable if version_probe(executable, &arguments) => true,
        GIT => git_is_read_only(&arguments),
        RG => !denies(&arguments, RG_DENIED_FLAGS) && no_attached_pattern_file(&arguments),
        GREP => {
            only_read_flags(&arguments, GREP_READ_FLAGS, GREP_READ_SHORT_FLAGS)
                && no_attached_pattern_file(&arguments)
        }
        FIND => !denies(&arguments, FIND_DENIED_FLAGS),
        SORT => only_read_flags(&arguments, SORT_READ_FLAGS, SORT_READ_SHORT_FLAGS),
        SED => sed_only_prints(&arguments),
        "date" => arguments.iter().all(|argument| {
            matches!(*argument, "-u" | "--utc" | "--universal") || argument.starts_with('+')
        }),
        "file" => only_read_flags(&arguments, FILE_READ_FLAGS, ""),
        "tree" => only_read_flags(&arguments, TREE_READ_FLAGS, ""),
        "du" => only_read_flags(&arguments, DU_READ_FLAGS, DU_READ_SHORT_FLAGS),
        "wc" => only_read_flags(&arguments, WC_READ_FLAGS, WC_READ_SHORT_FLAGS),
        "printf" => arguments
            .first()
            .is_some_and(|format| !format.starts_with('-') || *format == "--"),
        executable @ (LOOKUP_COMMAND | LOOKUP_TYPE) => name_lookup(executable, &arguments),
        executable if READ_ONLY_COMMANDS.contains(&executable) => true,
        _ => return Err(NotReadOnly::UnknownCommand),
    };
    if read_only {
        Ok(())
    } else {
        Err(NotReadOnly::UnsupportedInvocation)
    }
}

/// Whether every safe glob stands where its matches can only be paths to
/// read: among the operands of a `GLOB_READERS` command, after the pattern of
/// `grep` or `rg`, or as a pathspec after `--` for `git`.
fn globs_name_read_paths(executable: &str, arguments: &[&str], globs: &[Option<&str>]) -> bool {
    let Some(first) = globs.iter().position(Option::is_some) else {
        return true;
    };
    let mut before = arguments.iter().take(first);
    match executable {
        GREP | RG => before.any(|argument| !argument.starts_with('-')),
        GIT => before.any(|argument| *argument == END_OF_FLAGS),
        executable => GLOB_READERS.contains(&executable),
    }
}

fn version_probe(executable: &str, arguments: &[&str]) -> bool {
    let name = versioned_interpreter(executable).unwrap_or(executable);
    let [flag] = arguments else {
        return false;
    };
    VERSION_PROBES
        .iter()
        .any(|(probed, flags)| *probed == name && flags.contains(flag))
}

/// `command -v|-V NAME…` and `type [-afptP] NAME…` only report what each name
/// resolves to.
pub(crate) fn name_lookup(executable: &str, arguments: &[&str]) -> bool {
    let names = match executable {
        LOOKUP_COMMAND => match arguments {
            [flag, names @ ..] if LOOKUP_FLAGS.contains(flag) => names,
            _ => return false,
        },
        LOOKUP_TYPE => {
            let flags = arguments
                .iter()
                .take_while(|argument| {
                    argument.strip_prefix('-').is_some_and(|flags| {
                        !flags.is_empty() && flags.chars().all(|flag| TYPE_FLAGS.contains(flag))
                    })
                })
                .count();
            &arguments[flags..]
        }
        _ => return false,
    };
    !names.is_empty()
        && names
            .iter()
            .all(|name| !name.is_empty() && !name.starts_with('-'))
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
            && !git_read_value_flag(argument)
            && !argument.strip_prefix('-').is_some_and(decimal)
            && !argument.strip_prefix("--max-count=").is_some_and(decimal)
        {
            return false;
        }
    }
    true
}

fn git_read_value_flag(argument: &str) -> bool {
    GIT_READ_VALUE_FLAGS.iter().any(|flag| {
        argument.strip_prefix(flag).is_some_and(|value| {
            !GIT_FORMAT_FLAGS.contains(flag) || !value.contains(GIT_SIGNATURE_PLACEHOLDER)
        })
    })
}

pub(crate) fn decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Reads a flag list as an allow-list, the inverse of `denies`. A long flag may
/// carry its value attached, and a one-letter flag may travel inside a cluster,
/// so `--context=3` and `-in` are recognized without listing every spelling.
///
/// Abbreviations are deliberately not resolved: `--cont` costs a prompt rather
/// than an allowance, which is the safe direction for an allow-list and the
/// unsafe one for a deny-list. Everything past `--` is an operand by
/// definition, and operands are the confinement check's question, not this one's.
fn only_read_flags(arguments: &[&str], long: &[&str], short: &str) -> bool {
    arguments
        .iter()
        .take_while(|argument| **argument != END_OF_FLAGS)
        .all(|argument| {
            let Some(rest) = argument.strip_prefix('-').filter(|rest| !rest.is_empty()) else {
                return true;
            };
            if long.contains(argument)
                || argument
                    .split_once('=')
                    .is_some_and(|(name, _)| long.contains(&name))
            {
                return true;
            }
            !rest.starts_with('-') && rest.chars().all(|flag| short.contains(flag))
        })
}

fn no_attached_pattern_file(arguments: &[&str]) -> bool {
    arguments
        .iter()
        .all(|argument| *argument == "-f" || !hides_in_cluster(argument, "-f"))
}

pub(crate) fn confined_read(
    scope: &ShellCommandScope,
    globs: &[Option<&str>],
    incoming: &BashCwdSet,
    project: &Path,
) -> bool {
    let BashCwdSet::Known(directories) = incoming else {
        return false;
    };
    !directories.is_empty()
        && scope_is_read_only(scope, globs)
        && directories.iter().all(|directory| {
            directory.starts_with(project)
                && resolves_inside("", directory, project)
                && if scope.executable == CD {
                    cd_target(scope, directory, project).is_some()
                } else {
                    scope_stays_in_project(scope, globs, directory, project)
                }
        })
}

/// Where a `cd` leaves the shell, or `None` when the command does not say, or
/// says somewhere outside the project.
fn cd_target(scope: &ShellCommandScope, current: &Path, project: &Path) -> Option<PathBuf> {
    if !scope_is_read_only(scope, &[]) {
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

fn scope_stays_in_project(
    scope: &ShellCommandScope,
    globs: &[Option<&str>],
    workdir: &Path,
    project: &Path,
) -> bool {
    reviewed_arguments(scope, globs).is_some_and(|arguments| {
        arguments.iter().enumerate().all(|(index, argument)| {
            if glob_at(globs, index).is_some() {
                glob_stays_in_project(argument, workdir, project)
            } else {
                argument_stays_in_project(argument, workdir, project)
            }
        })
    })
}

/// Bash passes what a glob matches, or the pattern itself when nothing does,
/// so each of those has to stay inside. An expansion past its bounds is not
/// confined.
fn glob_stays_in_project(pattern: &str, workdir: &Path, project: &Path) -> bool {
    expand_glob(pattern, workdir).is_some_and(|matches| {
        if matches.is_empty() {
            resolves_inside(pattern, workdir, project)
        } else {
            matches
                .iter()
                .all(|path| resolves_inside(path, workdir, project))
        }
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
pub(crate) fn literal_arguments(scope: &ShellCommandScope) -> Option<Vec<&str>> {
    reviewed_arguments(scope, &[])
}

/// `literal_arguments`, with each safe glob standing as its own pattern.
fn reviewed_arguments<'a>(
    scope: &'a ShellCommandScope,
    globs: &[Option<&'a str>],
) -> Option<Vec<&'a str>> {
    scope
        .arguments
        .as_ref()?
        .iter()
        .enumerate()
        .map(|(index, word)| match word {
            ShellWord::Literal(text) => Some(text.as_str()),
            ShellWord::Undecodable => glob_at(globs, index),
        })
        .collect()
}

fn glob_at<'a>(globs: &[Option<&'a str>], index: usize) -> Option<&'a str> {
    globs.get(index).copied().flatten()
}

/// Text cannot see through a symlink, and a project can contain one pointing
/// anywhere, so an operand is resolved and bounded by the same symlink-aware
/// check the file tools use.
///
/// Most words are not paths at all, and they need no special case: the rules
/// above have already excluded every form that escapes, so a flag or a pattern
/// joins the workdir and canonicalizes to its own lexical form, which is inside.
/// Only a link can leave, and only resolving finds it.
fn resolves_inside(operand: impl AsRef<Path>, workdir: &Path, project: &Path) -> bool {
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

    use super::{
        FIND, GIT, NotReadOnly, RG, SED, confined_read, is_read_only, scope_is_read_only,
        shell_read_only_verdict,
    };
    use crate::pattern_analysis::shell_facts;
    use test_case::test_case;
    use workcell::shell::bash::{BashContextAssumptions, parse_bash};
    use workcell::shell::{ShellCommandAnalysis, ShellCommandScope, ShellWord};

    const PROJECT: &str = "/home/dev/project";
    const DENIAL_IS_LOAD_BEARING: &str = "a denied flag must keep disqualifying its command";
    const COMMAND_SEPARATOR: &str = " && ";

    #[test_case("git diff HEAD~1", Err(NotReadOnly::UndecodableWord); "unquoted_revision")]
    #[test_case("git diff 'HEAD~1'", Ok(()); "single_quoted_revision")]
    #[test_case("git diff \"HEAD~1\"", Ok(()); "double_quoted_revision")]
    #[test_case("find . -name *.rs", Err(NotReadOnly::UndecodableWord); "unquoted_glob")]
    #[test_case("find . -name '*.rs'", Ok(()); "quoted_glob")]
    #[test_case("rg a.*b src", Err(NotReadOnly::UndecodableWord); "unquoted_pattern")]
    #[test_case("rg 'a.*b' src", Ok(()); "quoted_pattern")]
    #[test_case("cat $HOME", Err(NotReadOnly::UndecodableWord); "expansion")]
    #[test_case("/bin/cat $HOME", Err(NotReadOnly::SourceMismatch); "source_mismatch_precedes_decoding")]
    #[test_case("/bin/cat notes.md", Err(NotReadOnly::SourceMismatch); "qualified_executable")]
    #[test_case("'cat' notes.md", Err(NotReadOnly::SourceMismatch); "quoted_executable")]
    #[test_case("diff a b", Err(NotReadOnly::UnknownCommand); "unknown_reader")]
    #[test_case("touch notes.md", Err(NotReadOnly::UnknownCommand); "unknown_writer")]
    #[test_case("env git status", Err(NotReadOnly::UnknownCommand); "environment_wrapper")]
    #[test_case("command git status", Err(NotReadOnly::UnsupportedInvocation); "command_wrapper")]
    #[test_case("bash -c 'git status'", Err(NotReadOnly::UnknownCommand); "shell_wrapper")]
    #[test_case("git push origin main", Err(NotReadOnly::UnsupportedInvocation); "writing_subcommand")]
    #[test_case("git clean -n", Err(NotReadOnly::UnsupportedInvocation); "unsupported_dry_run")]
    #[test_case("git", Err(NotReadOnly::UnsupportedInvocation); "missing_subcommand")]
    #[test_case("git log -n nope", Err(NotReadOnly::UnsupportedInvocation); "invalid_count_operand")]
    #[test_case("printf", Err(NotReadOnly::UnsupportedInvocation); "missing_format")]
    #[test_case("sed 's/a/b/' notes.md", Err(NotReadOnly::UnsupportedInvocation); "unsupported_script")]
    #[test_case("sed -i 's/a/b/' notes.md", Err(NotReadOnly::UnsupportedInvocation); "writing_script")]
    #[test_case("find . -delete", Err(NotReadOnly::UnsupportedInvocation); "denied_delete")]
    #[test_case("rg --follow needle", Err(NotReadOnly::UnsupportedInvocation); "denied_nonwriting_flag")]
    #[test_case("git status --short", Ok(()); "read_only_git")]
    #[test_case("git log -1 --format='%h %ci'", Ok(()); "git_log_format")]
    #[test_case("git log --date=short --since=2.weeks", Ok(()); "git_log_date_window")]
    #[test_case("git log --pretty --abbrev-commit --follow -- notes.md", Ok(()); "git_log_pretty_follow")]
    #[test_case("git log --pretty='format:%G?'", Err(NotReadOnly::UnsupportedInvocation); "git_signature_placeholder_runs_gpg")]
    #[test_case("git log --format=%GS", Err(NotReadOnly::UnsupportedInvocation); "git_signer_placeholder_runs_gpg")]
    #[test_case("git log --show-signature", Err(NotReadOnly::UnsupportedInvocation); "git_show_signature_runs_gpg")]
    #[test_case("git log --format '%h'", Err(NotReadOnly::UnsupportedInvocation); "git_detached_format_value")]
    #[test_case("git diff --ext-diff", Err(NotReadOnly::UnsupportedInvocation); "git_external_diff")]
    #[test_case("cat notes.md > out.txt", Ok(()); "redirect_is_not_a_scope_fact")]
    #[test_case("cat .env", Ok(()); "protected_path_is_not_a_scope_fact")]
    #[test_case("cat /etc/shadow", Ok(()); "confinement_is_not_a_scope_fact")]
    #[test_case("ls -la src/*", Ok(()); "a_glob_listing")]
    #[test_case("echo src/*", Err(NotReadOnly::UndecodableWord); "echo_prints_the_names_a_glob_matches")]
    #[test_case("rg needle src/*", Ok(()); "a_glob_after_the_search_pattern")]
    #[test_case("grep -n needle src/*", Ok(()); "a_glob_after_the_grep_pattern")]
    #[test_case("rg -n src/* notes.md", Err(NotReadOnly::UndecodableWord); "a_glob_that_could_be_the_search_pattern")]
    #[test_case("git log -- src/*", Ok(()); "a_glob_pathspec")]
    #[test_case("git log src/*", Err(NotReadOnly::UndecodableWord); "a_glob_that_could_be_a_revision")]
    #[test_case("find src/* -name x", Err(NotReadOnly::UndecodableWord); "a_glob_for_a_reader_with_an_expression")]
    fn parsed_scope_verdicts(command: &str, expected: Result<(), NotReadOnly>) {
        assert_eq!(parsed_verdict(command), expected);
    }

    fn parsed_verdict(command: &str) -> Result<(), NotReadOnly> {
        let program = parse_bash(command).unwrap();
        let contexts = program.command_contexts(Path::new(PROJECT));
        let facts = shell_facts(&program, &contexts);
        let [command] = facts.commands.as_slice() else {
            panic!("expected one command, got {}", facts.commands.len());
        };
        let verdict = shell_read_only_verdict(&command.scope, &command.globs);
        assert_eq!(
            scope_is_read_only(&command.scope, &command.globs),
            verdict.is_ok()
        );
        verdict
    }

    #[test_case("python --version", Ok(()); "python")]
    #[test_case("python3 -V", Ok(()); "python3")]
    #[test_case("python3.12 --version", Ok(()); "versioned_python")]
    #[test_case("node -v", Ok(()); "node")]
    #[test_case("npm --version", Ok(()); "npm")]
    #[test_case("pnpm -v", Ok(()); "pnpm")]
    #[test_case("yarn -v", Ok(()); "yarn")]
    #[test_case("deno --version", Ok(()); "deno")]
    #[test_case("bun --version", Ok(()); "bun")]
    #[test_case("cargo -V", Ok(()); "cargo")]
    #[test_case("rustc --version", Ok(()); "rustc")]
    #[test_case("rustup -V", Ok(()); "rustup")]
    #[test_case("go version", Ok(()); "go")]
    #[test_case("java -version", Ok(()); "java")]
    #[test_case("ruby --version", Ok(()); "ruby")]
    #[test_case("perl -v", Ok(()); "perl")]
    #[test_case("gcc --version", Ok(()); "gcc")]
    #[test_case("clang --version", Ok(()); "clang")]
    #[test_case("make --version", Ok(()); "make")]
    #[test_case("cmake --version", Ok(()); "cmake")]
    #[test_case("git --version", Ok(()); "git")]
    #[test_case("just --version", Ok(()); "just")]
    #[test_case("rg --version", Ok(()); "ripgrep")]
    #[test_case("jq --version", Ok(()); "jq")]
    #[test_case("uv --version", Ok(()); "uv")]
    #[test_case("pip --version", Ok(()); "pip")]
    #[test_case("pip3 --version", Ok(()); "pip3")]
    #[test_case("nix --version", Ok(()); "nix")]
    #[test_case("docker --version", Ok(()); "docker")]
    #[test_case("python3 -v", Err(NotReadOnly::UnknownCommand); "python_verbose_repl")]
    #[test_case("ruby -v", Err(NotReadOnly::UnknownCommand); "ruby_reading_stdin")]
    #[test_case("go --version", Err(NotReadOnly::UnknownCommand); "another_tools_flag")]
    #[test_case("./python3 --version", Err(NotReadOnly::SourceMismatch); "qualified_interpreter")]
    #[test_case("python3 --version script.py", Err(NotReadOnly::UnknownCommand); "an_extra_operand")]
    #[test_case("cargo --version --verbose", Err(NotReadOnly::UnknownCommand); "an_extra_flag")]
    #[test_case("git --version --build-options", Err(NotReadOnly::UnsupportedInvocation); "an_extra_flag_to_a_known_reader")]
    fn version_probes_are_read_only(command: &str, expected: Result<(), NotReadOnly>) {
        assert_eq!(parsed_verdict(command), expected);
    }

    #[test_case("command -v rg", Ok(()); "command_lookup")]
    #[test_case("command -V rg cargo", Ok(()); "described_lookup_of_several_names")]
    #[test_case("type rg", Ok(()); "type_lookup")]
    #[test_case("type -t rg", Ok(()); "type_kind")]
    #[test_case("type -a -P rg", Ok(()); "type_separate_flags")]
    #[test_case("type -ap rg", Ok(()); "type_clustered_flags")]
    #[test_case("command rg --version", Err(NotReadOnly::UnsupportedInvocation); "command_running_a_command")]
    #[test_case("command -p rg", Err(NotReadOnly::UnsupportedInvocation); "command_with_the_default_path")]
    #[test_case("command -v", Err(NotReadOnly::UnsupportedInvocation); "a_lookup_without_a_name")]
    #[test_case("command -v -p", Err(NotReadOnly::UnsupportedInvocation); "a_flag_where_a_name_belongs")]
    #[test_case("type -x rg", Err(NotReadOnly::UnsupportedInvocation); "an_unknown_type_flag")]
    #[test_case("type", Err(NotReadOnly::UnsupportedInvocation); "type_without_a_name")]
    #[test_case("command -v $TOOL", Err(NotReadOnly::UndecodableWord); "a_name_that_is_not_literal")]
    #[test_case("/usr/bin/command -v rg", Err(NotReadOnly::SourceMismatch); "a_qualified_lookup")]
    fn name_lookups_are_read_only(command: &str, expected: Result<(), NotReadOnly>) {
        assert_eq!(parsed_verdict(command), expected);
    }

    #[test_case(None; "unenumerated_arguments")]
    #[test_case(Some(vec![ShellWord::Undecodable]); "undecodable_argument")]
    fn unavailable_words_have_a_decoding_reason(arguments: Option<Vec<ShellWord>>) {
        assert_eq!(
            shell_read_only_verdict(&scope("pwd", arguments), &[]),
            Err(NotReadOnly::UndecodableWord)
        );
    }

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
                    .is_some_and(|context| confined_read(scope, &[], &context.incoming, project))
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
    #[test_case("rg -iz pattern" => false ; "ripgrep_search_zip_inside_a_cluster")]
    #[test_case("git --no-pager diff" => true ; "a_long_flag_is_not_a_cluster")]
    #[test_case("grep -rn needle src" => true ; "grep_recursively")]
    #[test_case("grep -R needle src" => false ; "grep_dereferencing_every_symlink")]
    #[test_case("grep -Rn needle src" => false ; "grep_dereferencing_from_inside_a_cluster")]
    #[test_case("find . -name *.rs" => true ; "find_by_name")]
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
    #[test_case("grep --context=3 needle src" => true ; "grep_attached_context")]
    #[test_case("grep -- -needle src" => true ; "grep_end_of_options")]
    #[test_case("grep --devices=read needle src" => false ; "grep_reading_devices")]
    #[test_case("grep --exclude-from=list needle src" => false ; "grep_unreviewed_exclusions")]
    #[test_case("grep --file patterns needle" => false ; "grep_unreviewed_pattern_file")]
    #[test_case("wc -l Cargo.toml" => true ; "wc_counting_lines")]
    #[test_case("wc --files0-from=paths" => false ; "wc_indirect_operands")]
    #[test_case("du -sh ." => true ; "du_summarizing")]
    #[test_case("du -L ." => false ; "du_following_every_symlink")]
    #[test_case("du --exclude-from=list ." => false ; "du_unreviewed_exclusions")]
    fn single_commands_are_classified(command: &str) -> bool {
        is_read_only(&analysis(&[command]), false)
    }

    /// `find` and `rg` keep deny-lists because their expression grammars are
    /// too large to enumerate, which makes every entry load-bearing on its own.
    /// A loop over the list would pin nothing, since dropping an entry drops
    /// its case with it, so each flag is named here as a command of its own.
    #[test_case("find . -delete" ; "find_delete")]
    #[test_case("find . -exec rm x ;" ; "find_exec")]
    #[test_case("find . -execdir rm x ;" ; "find_execdir")]
    #[test_case("find . -fls listing" ; "find_file_listing")]
    #[test_case("find . -fprint listing" ; "find_file_print")]
    #[test_case("find . -fprint0 listing" ; "find_null_file_print")]
    #[test_case("find . -fprintf listing %p" ; "find_formatted_file_print")]
    #[test_case("find . -ok rm x ;" ; "find_confirmed_exec")]
    #[test_case("find . -okdir rm x ;" ; "find_confirmed_execdir")]
    #[test_case("find -L . -name x" ; "find_follow_links")]
    #[test_case("find -H . -name x" ; "find_follow_argument_links")]
    #[test_case("find . -files0-from paths" ; "find_indirect_operands")]
    #[test_case("rg --hostname-bin ./host needle" ; "ripgrep_hostname_program")]
    #[test_case("rg --pre ./run.sh needle" ; "ripgrep_preprocessor")]
    #[test_case("rg --pre-glob *.gz needle" ; "ripgrep_preprocessor_glob")]
    #[test_case("rg --search-zip needle" ; "ripgrep_search_zip")]
    #[test_case("rg --follow needle" ; "ripgrep_follow_links")]
    #[test_case("rg -L needle" ; "ripgrep_short_follow_links")]
    #[test_case("rg -z needle" ; "ripgrep_short_search_zip")]
    fn a_denied_flag_disqualifies_its_command(command: &str) {
        assert!(
            !is_read_only(&analysis(&[command]), false),
            "{DENIAL_IS_LOAD_BEARING}"
        );
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
    #[cfg_attr(unix, test_case(&["cat notes.md"] => false ; "a symlink to a file outside it"))]
    #[cfg_attr(unix, test_case(&["cat linked/id_rsa"] => false ; "a path through a symlinked directory"))]
    #[test_case(&["cat missing.md"] => true ; "a name that resolves to nothing is not a path we read")]
    #[test_case(&["find . -name *.rs"] => true ; "a pattern argument still resolves to nothing")]
    #[cfg_attr(unix, test_case(&["cd linked"] => false ; "a move through a symlinked directory"))]
    // `sub/escape` leaves the project and `escape` names nothing, so the answer
    // is only right when the operand is resolved against the directory the `cd`
    // reached. Against the workdir it resolves to nothing and reads as confined.
    #[cfg_attr(unix, test_case(&["cd sub", "cat escape"] => false ; "a link the move brings into reach"))]
    fn a_symlink_out_of_the_project_is_not_confined(commands: &[&str]) -> bool {
        let project = tempfile::tempdir().expect("project");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "key").expect("secret");
        std::fs::write(project.path().join("inside.md"), "notes").expect("inside");
        std::fs::create_dir(project.path().join("sub")).expect("subdirectory");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, project.path().join("notes.md"))
                .expect("file link");
            std::os::unix::fs::symlink(outside.path(), project.path().join("linked"))
                .expect("dir link");
            std::os::unix::fs::symlink(&secret, project.path().join("sub/escape"))
                .expect("nested link");
        }
        let root = project.path().canonicalize().expect("canonical project");

        line_is_confined(&analysis(commands), &root, &root)
    }

    /// The bound is the project, not the directory the command runs from, so a
    /// link that never leaves the project stays confined even when it points
    /// outside the workdir.
    #[cfg(unix)]
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
