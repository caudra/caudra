//! Names what a shell line can change, so its change record covers exactly that.
//!
//! A line whose every command only reads records nothing, wherever it reads. A
//! line whose writes are all spelled out, as a known writer's operands or as a
//! redirect to a literal file, records those paths. Anything else, including
//! every line the analysis cannot account for, compares the whole session
//! directory before and after the call.

use std::collections::BTreeSet;
use std::path::Path;
use std::slice;

use caudra_agent::permissions::sed_written_files;
use caudra_workspace::{RecordScope, RecordedPath, UNREVEALED_ROOT, WorkspacePath};
use workcell::shell::bash::{BashCommandContexts, BashCwdSet, BashProgram};
use workcell::shell::{PreparedShell, ShellCommandScope};

use crate::pattern_analysis::{RedirectEffect, redirect_effect, shell_facts};
use crate::read_only_shell::{literal_arguments, scope_is_read_only};

const END_OF_OPTIONS: &str = "--";
const STANDARD_STREAM: &str = "-";
const SED: &str = "sed";
/// No option listed here takes a value, so every other word is a path. `-t DIR`
/// and the options that name a reference file are left out, which sends those
/// forms to a whole-directory comparison.
const WRITERS: &[Writer] = &[
    Writer {
        name: "rm",
        short_flags: "fdrRv",
        long_flags: &["--force", "--dir", "--recursive", "--verbose"],
        writes: Writes::Operands,
    },
    Writer {
        name: "mv",
        short_flags: "fnvT",
        long_flags: &[
            "--force",
            "--no-clobber",
            "--verbose",
            "--no-target-directory",
        ],
        writes: Writes::Operands,
    },
    Writer {
        name: "cp",
        short_flags: "fnvrRapT",
        long_flags: &[
            "--force",
            "--no-clobber",
            "--verbose",
            "--recursive",
            "--archive",
            "--no-target-directory",
        ],
        writes: Writes::Destination,
    },
    Writer {
        name: "ln",
        short_flags: "sfnvT",
        long_flags: &[
            "--symbolic",
            "--force",
            "--no-dereference",
            "--verbose",
            "--no-target-directory",
        ],
        writes: Writes::Destination,
    },
    Writer {
        name: "touch",
        short_flags: "amc",
        long_flags: &["--no-create"],
        writes: Writes::Operands,
    },
    Writer {
        name: "mkdir",
        short_flags: "pv",
        long_flags: &["--parents", "--verbose"],
        writes: Writes::Operands,
    },
    Writer {
        name: "tee",
        short_flags: "aip",
        long_flags: &["--append", "--ignore-interrupts"],
        writes: Writes::Operands,
    },
];

/// What is known about paths outside the session directory.
#[derive(Clone, Copy, Debug)]
enum Surroundings {
    /// The line's paths are the host's own, so a path outside the session
    /// directory is somewhere else and changes nothing in it.
    Known,
    /// Only paths relative to the session directory can be placed, so an
    /// absolute path could still name a file inside it.
    Unknown,
}

struct Writer {
    name: &'static str,
    short_flags: &'static str,
    long_flags: &'static [&'static str],
    writes: Writes,
}

enum Writes {
    Operands,
    /// The last operand, which the others are copied or linked into.
    Destination,
}

impl Writer {
    fn written(&self, arguments: &[&str]) -> Option<Vec<String>> {
        let mut operands = Vec::new();
        let mut options_ended = false;
        for argument in arguments {
            if options_ended || *argument == STANDARD_STREAM || !argument.starts_with('-') {
                operands.push(*argument);
            } else if *argument == END_OF_OPTIONS {
                options_ended = true;
            } else if argument.starts_with(END_OF_OPTIONS) {
                if !self.long_flags.contains(argument) {
                    return None;
                }
            } else if !argument[1..]
                .chars()
                .all(|flag| self.short_flags.contains(flag))
            {
                return None;
            }
        }
        let written = match self.writes {
            Writes::Operands => operands.as_slice(),
            Writes::Destination => match operands.split_last() {
                Some((destination, sources)) if !sources.is_empty() => slice::from_ref(destination),
                _ => return None,
            },
        };
        Some(
            written
                .iter()
                .map(|operand| (*operand).to_owned())
                .collect(),
        )
    }
}

/// The record a line prepared on this machine needs in the session directory at
/// `root`. A line nobody could parse may write anywhere.
pub(crate) fn prepared_shell_record_scope(
    shell: &PreparedShell,
    root: &Path,
) -> Option<RecordScope> {
    match (shell.bash_program(), shell.bash_command_contexts()) {
        (Ok(program), Ok(contexts)) => {
            shell_record_scope(program, &contexts, root, Surroundings::Known)
        }
        _ => Some(RecordScope::Workspace),
    }
}

/// The record a line a remote host runs needs, its contexts taken under
/// [`UNREVEALED_ROOT`].
pub(crate) fn remote_shell_record_scope(
    program: &BashProgram,
    contexts: &BashCommandContexts,
) -> Option<RecordScope> {
    shell_record_scope(
        program,
        contexts,
        Path::new(UNREVEALED_ROOT),
        Surroundings::Unknown,
    )
}

/// The record a shell line needs, or `None` when it writes nothing inside the
/// session directory at `root`.
fn shell_record_scope(
    program: &BashProgram,
    contexts: &BashCommandContexts,
    root: &Path,
    surroundings: Surroundings,
) -> Option<RecordScope> {
    match named_writes(program, contexts, root, surroundings) {
        None => Some(RecordScope::Workspace),
        Some(paths) if paths.is_empty() => None,
        Some(paths) => Some(RecordScope::Paths(paths)),
    }
}

/// Every path inside the session directory the line can write, or `None` when
/// one of its writes is not spelled out.
fn named_writes(
    program: &BashProgram,
    contexts: &BashCommandContexts,
    root: &Path,
    surroundings: Surroundings,
) -> Option<BTreeSet<WorkspacePath>> {
    let facts = shell_facts(program, contexts);
    if facts.unaccounted {
        return None;
    }
    let mut paths = BTreeSet::new();
    for command in &facts.commands {
        let BashCwdSet::Known(workdirs) = &command.context?.incoming else {
            return None;
        };
        let mut targets = written_operands(&command.scope)?;
        targets.extend(command.command.redirects.iter().filter_map(
            |redirect| match redirect_effect(program, redirect) {
                RedirectEffect::Writes(target) => Some(target.to_owned()),
                _ => None,
            },
        ));
        for workdir in workdirs {
            for target in &targets {
                paths.extend(place(target, workdir, root, surroundings)?);
            }
        }
    }
    Some(paths)
}

/// The operands a command writes, or `None` when it could write anything else.
fn written_operands(scope: &ShellCommandScope) -> Option<Vec<String>> {
    if scope_is_read_only(scope) {
        return Some(Vec::new());
    }
    if scope.source != scope.normalized {
        return None;
    }
    let arguments = literal_arguments(scope)?;
    if scope.executable == SED {
        return sed_written_files(&arguments);
    }
    WRITERS
        .iter()
        .find(|writer| writer.name == scope.executable)?
        .written(&arguments)
}

/// Where a written target lands: `Some(None)` outside the session directory,
/// and `None` when the text cannot say.
fn place(
    target: &str,
    workdir: &Path,
    root: &Path,
    surroundings: Surroundings,
) -> Option<Option<WorkspacePath>> {
    match RecordedPath::of(&workdir.join(target), root) {
        RecordedPath::Inside(path) => Some(Some(path)),
        RecordedPath::Outside => match surroundings {
            Surroundings::Known => Some(None),
            Surroundings::Unknown => None,
        },
        RecordedPath::Unplaced => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use caudra_workspace::{RecordScope, WorkspacePath};
    use test_case::test_case;
    use workcell::shell::bash::{BashContextAssumptions, parse_bash};

    use super::{Surroundings, shell_record_scope};

    const PROJECT: &str = "/home/dev/project";
    const PARSE_EXPECTED: &str = "the fixture parses";
    const PATH_EXPECTED: &str = "the fixture names a workspace path";
    const WORKSPACE: Option<RecordScope> = Some(RecordScope::Workspace);

    fn assumptions() -> BashContextAssumptions {
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
        }
    }

    fn written(paths: &[&str]) -> Option<RecordScope> {
        Some(RecordScope::Paths(
            paths
                .iter()
                .map(|path| WorkspacePath::new(*path).expect(PATH_EXPECTED))
                .collect(),
        ))
    }

    fn scope_of(command: &str, surroundings: Surroundings) -> Option<RecordScope> {
        let program = parse_bash(command).expect(PARSE_EXPECTED);
        let project = Path::new(PROJECT);
        let contexts = program.command_contexts_with_assumptions(project, assumptions());
        shell_record_scope(&program, &contexts, project, surroundings)
    }

    #[test_case("rm -rf target/debug notes.md" => written(&["notes.md", "target/debug"]) ; "rm_operands")]
    #[test_case("rm -- -notes.md" => written(&["-notes.md"]) ; "an_operand_after_the_end_of_options")]
    #[test_case("rm -" => written(&["-"]) ; "a_lone_dash_is_an_operand")]
    #[test_case("mv -f old.rs new.rs" => written(&["new.rs", "old.rs"]) ; "mv_source_and_destination")]
    #[test_case("cp -a src backup" => written(&["backup"]) ; "cp_destination_only")]
    #[test_case("ln -sf ../shared.toml config.toml" => written(&["config.toml"]) ; "ln_link_name_only")]
    #[test_case("touch -c a.txt b.txt" => written(&["a.txt", "b.txt"]) ; "touch_operands")]
    #[test_case("mkdir -p src/deep/dir" => written(&["src/deep/dir"]) ; "mkdir_operands")]
    #[test_case("echo x | tee -a log.txt" => written(&["log.txt"]) ; "tee_in_a_pipeline")]
    #[test_case("sed -i.bak 's/a/b/' lib.rs" => written(&["lib.rs", "lib.rs.bak"]) ; "sed_in_place_with_a_backup")]
    #[test_case("sed 's/a/b/' lib.rs > out.rs" => written(&["out.rs"]) ; "sed_to_standard_output")]
    #[test_case("echo x > out.txt" => written(&["out.txt"]) ; "write_redirect")]
    #[test_case("echo x >> out.txt" => written(&["out.txt"]) ; "append_redirect")]
    #[test_case("echo x >| out.txt" => written(&["out.txt"]) ; "clobber_redirect")]
    #[test_case("ls &> out.txt" => written(&["out.txt"]) ; "write_both_redirect")]
    #[test_case("ls &>> out.txt" => written(&["out.txt"]) ; "append_both_redirect")]
    #[test_case("ls 2> errors.txt" => written(&["errors.txt"]) ; "numbered_redirect")]
    #[test_case("rm ./notes.md /home/dev/project/src/lib.rs" => written(&["notes.md", "src/lib.rs"]) ; "spellings_of_paths_inside")]
    #[test_case("cd src && rm lib.rs" => written(&["src/lib.rs"]) ; "a_target_resolves_where_the_command_runs")]
    #[test_case("cd a || cd b; rm x" => written(&["a/x", "b/x", "x"]) ; "every_directory_the_command_can_run_in")]
    #[test_case("ls /tmp" => None ; "a_read_anywhere")]
    #[test_case("git status --short" => None ; "a_read_only_git_command")]
    #[test_case("echo x > /dev/null 2>&1" => None ; "the_null_device_and_descriptors")]
    #[test_case("cat < notes.md" => None ; "a_read_redirect")]
    #[test_case("echo x > /tmp/out.txt" => None ; "a_redirect_outside")]
    #[test_case("cd /tmp && touch x" => None ; "a_writer_outside")]
    #[test_case("rm *.o" => WORKSPACE ; "a_glob")]
    #[test_case("rm $X" => WORKSPACE ; "a_variable")]
    #[test_case("rm $(cat list)" => WORKSPACE ; "a_substitution")]
    #[test_case("echo x > \"$OUT\"" => WORKSPACE ; "a_redirect_to_a_variable")]
    #[test_case("xargs rm" => WORKSPACE ; "a_wrapper")]
    #[test_case("find . -delete" => WORKSPACE ; "a_reader_that_can_write")]
    #[test_case("sed -i 's/a/b/w out' f" => WORKSPACE ; "a_sed_script_that_writes")]
    #[test_case("git checkout -- f" => WORKSPACE ; "a_writing_git_command")]
    #[test_case("cargo fmt" => WORKSPACE ; "an_unknown_program")]
    #[test_case("cat > notes.md <<EOF\nhi\nEOF" => WORKSPACE ; "a_heredoc")]
    #[test_case("rm -i x" => WORKSPACE ; "an_unlisted_flag")]
    #[test_case("rm --no-preserve-root -rf x" => WORKSPACE ; "an_unlisted_long_flag")]
    #[test_case("cp -t dir a" => WORKSPACE ; "a_target_directory_option")]
    #[test_case("cp a" => WORKSPACE ; "a_copy_without_a_destination")]
    #[test_case("/bin/rm x" => WORKSPACE ; "a_qualified_executable")]
    #[test_case("FORCE=1 rm x" => WORKSPACE ; "an_assignment")]
    #[test_case("rm ../x" => WORKSPACE ; "a_parent_directory")]
    #[test_case("rm /home/dev/elsewhere/../project/x" => WORKSPACE ; "a_path_that_climbs_back_inside")]
    #[test_case("rm -rf ." => WORKSPACE ; "the_session_directory_itself")]
    fn lines_are_scoped(command: &str) -> Option<RecordScope> {
        scope_of(command, Surroundings::Known)
    }

    #[test_case("echo x > /tmp/out.txt" => WORKSPACE ; "an_absolute_redirect")]
    #[test_case("cd /tmp && touch x" => WORKSPACE ; "an_absolute_directory")]
    #[test_case("rm x 2>/dev/null" => written(&["x"]) ; "relative_targets_and_the_null_device")]
    fn absolute_paths_are_unplaced_when_the_surroundings_are_unknown(
        command: &str,
    ) -> Option<RecordScope> {
        scope_of(command, Surroundings::Unknown)
    }
}
