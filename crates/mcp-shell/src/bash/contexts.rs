use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use serde::Serialize;

use super::{BashCommand, BashNodeId, BashNodeKind, BashOperatorKind, BashParseError, BashProgram};

pub const MAX_CWD_STATES: usize = 32;
pub const MAX_CWD_PATH_BYTES: usize = 4096;
pub const MAX_CONTEXT_BYTES: usize = 1024 * 1024;
const CONTEXT_DIAGNOSTICS_PER_NODE: usize = 3;
const INITIAL_CONTEXT_DIAGNOSTICS: usize = 3;
const BASH_BUILTINS: &[&str] = &[
    ".",
    ":",
    "[",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "compopt",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum BashContextError {
    UnsupportedLauncher,
    Parse(BashParseError),
}

impl fmt::Display for BashContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedLauncher => formatter
                .write_str("The prepared shell does not use the deterministic Bash launcher"),
            Self::Parse(error) => error.fmt(formatter),
        }
    }
}

impl Error for BashContextError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UnsupportedLauncher => None,
            Self::Parse(error) => Some(error),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BashContextAssumptions {
    pub startup_preserves_cwd: bool,
    pub no_aliases_functions_or_command_not_found_hook: bool,
    pub no_traps: bool,
    pub default_shell_options: bool,
    pub standard_builtins: bool,
    pub directory_variables_are_standard: bool,
    pub cdpath_empty: bool,
    pub lastpipe_disabled: bool,
    pub logical_pwd_matches_initial: bool,
}

impl BashContextAssumptions {
    fn declared(&self) -> bool {
        self.startup_preserves_cwd
            && self.no_aliases_functions_or_command_not_found_hook
            && self.no_traps
            && self.default_shell_options
            && self.standard_builtins
            && self.directory_variables_are_standard
            && self.cdpath_empty
            && self.lastpipe_disabled
            && self.logical_pwd_matches_initial
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "symbolic_paths", rename_all = "snake_case")]
pub enum BashCwdSet {
    Known(Vec<PathBuf>),
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashContextIssue {
    UndeclaredShellAssumptions,
    InvalidInitialCwd,
    IncompleteProgram,
    UnknownStateEffect,
    StateLimit,
    PathLimit,
    RetainedBytesLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashContextDiagnostic {
    pub node: BashNodeId,
    pub issue: BashContextIssue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashCommandContext {
    pub command: BashNodeId,
    pub incoming: BashCwdSet,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashCommandContexts {
    pub assumptions: BashContextAssumptions,
    pub commands: Vec<BashCommandContext>,
    pub on_success: BashCwdSet,
    pub on_failure: BashCwdSet,
    pub complete: bool,
    pub diagnostics: Vec<BashContextDiagnostic>,
}

struct Outcomes {
    success: BashCwdSet,
    failure: BashCwdSet,
}

impl Outcomes {
    fn unchanged(incoming: BashCwdSet) -> Self {
        Self {
            success: incoming.clone(),
            failure: incoming,
        }
    }
}

struct ContextBuilder {
    result: BashCommandContexts,
    retained_bytes: usize,
}

impl BashProgram {
    pub fn command_contexts(&self, initial_cwd: &Path) -> BashCommandContexts {
        self.command_contexts_with_assumptions(initial_cwd, BashContextAssumptions::default())
    }

    pub fn command_contexts_with_assumptions(
        &self,
        initial_cwd: &Path,
        assumptions: BashContextAssumptions,
    ) -> BashCommandContexts {
        let commands = Vec::with_capacity(self.commands().count());
        let diagnostics = Vec::with_capacity(
            self.nodes.len() * CONTEXT_DIAGNOSTICS_PER_NODE + INITIAL_CONTEXT_DIAGNOSTICS,
        );
        let retained_bytes = size_of::<BashCommandContexts>()
            + commands.capacity() * size_of::<BashCommandContext>()
            + diagnostics.capacity() * size_of::<BashContextDiagnostic>();
        let mut builder = ContextBuilder {
            result: BashCommandContexts {
                assumptions,
                commands,
                on_success: BashCwdSet::Unknown,
                on_failure: BashCwdSet::Unknown,
                complete: true,
                diagnostics,
            },
            retained_bytes,
        };
        let mut incoming = BashCwdSet::Unknown;
        if initial_cwd.is_absolute() && initial_cwd.as_os_str().len() <= MAX_CWD_PATH_BYTES {
            incoming = BashCwdSet::Known(vec![initial_cwd.to_owned()]);
        }
        if !builder.result.assumptions.declared() {
            builder.issue(self.root, BashContextIssue::UndeclaredShellAssumptions);
            incoming = BashCwdSet::Unknown;
        }
        if !initial_cwd.is_absolute() || initial_cwd.as_os_str().len() > MAX_CWD_PATH_BYTES {
            builder.issue(self.root, BashContextIssue::InvalidInitialCwd);
            incoming = BashCwdSet::Unknown;
        }
        if !self.is_complete() {
            builder.issue(self.root, BashContextIssue::IncompleteProgram);
            incoming = BashCwdSet::Unknown;
        }
        let outcomes = builder.visit(self, self.root, incoming);
        builder.result.on_success = builder.retain(self.root, outcomes.success);
        builder.result.on_failure = builder.retain(self.root, outcomes.failure);
        builder
            .result
            .commands
            .sort_by_key(|context| self.nodes[context.command.0].span.start);
        builder.result
    }
}

impl ContextBuilder {
    fn issue(&mut self, node: BashNodeId, issue: BashContextIssue) {
        self.result.complete = false;
        let diagnostic = BashContextDiagnostic { node, issue };
        if !self.result.diagnostics.contains(&diagnostic) {
            self.result.diagnostics.push(diagnostic);
        }
    }

    fn join(&mut self, node: BashNodeId, left: BashCwdSet, right: BashCwdSet) -> BashCwdSet {
        match (left, right) {
            (BashCwdSet::Known(mut left), BashCwdSet::Known(right)) => {
                left.extend(right);
                left.sort();
                left.dedup();
                if left.len() > MAX_CWD_STATES {
                    self.issue(node, BashContextIssue::StateLimit);
                    BashCwdSet::Unknown
                } else {
                    BashCwdSet::Known(left)
                }
            }
            _ => BashCwdSet::Unknown,
        }
    }

    fn retain(&mut self, node: BashNodeId, states: BashCwdSet) -> BashCwdSet {
        if let BashCwdSet::Known(paths) = &states {
            let bytes = paths
                .capacity()
                .saturating_mul(size_of::<PathBuf>())
                .saturating_add(
                    paths
                        .iter()
                        .map(PathBuf::capacity)
                        .fold(0, usize::saturating_add),
                );
            self.retained_bytes = self.retained_bytes.saturating_add(bytes);
            if self.retained_bytes > MAX_CONTEXT_BYTES {
                self.issue(node, BashContextIssue::RetainedBytesLimit);
                return BashCwdSet::Unknown;
            }
        }
        states
    }

    fn visit(&mut self, program: &BashProgram, id: BashNodeId, incoming: BashCwdSet) -> Outcomes {
        match &program.nodes[id.0].structure {
            BashNodeKind::Sequence { items, .. } => {
                let mut outcomes = Outcomes::unchanged(incoming);
                for item in items {
                    let joined = self.join(*item, outcomes.success, outcomes.failure);
                    outcomes = self.visit(program, *item, joined);
                }
                outcomes
            }
            BashNodeKind::AndOr {
                left,
                operator,
                right,
            } => {
                let left = self.visit(program, *left, incoming);
                if operator.kind == BashOperatorKind::And {
                    let right = self.visit(program, *right, left.success);
                    Outcomes {
                        success: right.success,
                        failure: self.join(id, left.failure, right.failure),
                    }
                } else {
                    let right = self.visit(program, *right, left.failure);
                    Outcomes {
                        success: self.join(id, left.success, right.success),
                        failure: right.failure,
                    }
                }
            }
            BashNodeKind::Pipeline { commands, .. } => {
                for command in commands {
                    self.visit(program, *command, incoming.clone());
                }
                Outcomes::unchanged(incoming)
            }
            BashNodeKind::Subshell { body, .. } | BashNodeKind::Background { body, .. } => {
                self.visit(program, *body, incoming.clone());
                Outcomes::unchanged(incoming)
            }
            BashNodeKind::BraceGroup { body, .. } => self.visit(program, *body, incoming),
            BashNodeKind::Command { command } => {
                let incoming = self.retain(id, incoming);
                self.result.commands.push(BashCommandContext {
                    command: id,
                    complete: matches!(incoming, BashCwdSet::Known(_)),
                    incoming: incoming.clone(),
                });
                self.command(id, command, incoming)
            }
            BashNodeKind::Assignments { .. } | BashNodeKind::Unknown { .. } => {
                self.issue(id, BashContextIssue::UnknownStateEffect);
                Outcomes::unchanged(BashCwdSet::Unknown)
            }
        }
    }

    fn command(&mut self, id: BashNodeId, command: &BashCommand, incoming: BashCwdSet) -> Outcomes {
        let Some(argv) = command.static_argv() else {
            return self.unknown_effect(id);
        };
        if !command.assignments.is_empty()
            || command.redirects.iter().any(|redirect| {
                redirect
                    .target
                    .as_ref()
                    .is_some_and(|target| target.literal.is_none())
            })
        {
            return self.unknown_effect(id);
        }
        let Some(executable) = argv.first() else {
            return self.unknown_effect(id);
        };
        if *executable == "cd" {
            let operand = match argv.as_slice() {
                [_, operand] if !operand.starts_with('-') && !operand.is_empty() => *operand,
                [_, "--", operand] if *operand != "-" && !operand.is_empty() => *operand,
                _ => return self.unknown_effect(id),
            };
            if operand
                .split('/')
                .any(|component| matches!(component, "." | ".."))
            {
                return self.unknown_effect(id);
            }
            let success = match &incoming {
                BashCwdSet::Known(paths) => {
                    let mut changed = Vec::new();
                    for path in paths {
                        let next = path.join(operand);
                        if next.as_os_str().len() > MAX_CWD_PATH_BYTES {
                            self.issue(id, BashContextIssue::PathLimit);
                            return Outcomes {
                                success: BashCwdSet::Unknown,
                                failure: incoming,
                            };
                        }
                        changed.push(next);
                    }
                    changed.sort();
                    changed.dedup();
                    BashCwdSet::Known(changed)
                }
                BashCwdSet::Unknown => BashCwdSet::Unknown,
            };
            return Outcomes {
                success,
                failure: incoming,
            };
        }
        if BASH_BUILTINS.contains(executable) {
            return match *executable {
                ":" | "true" | "false" | "echo" => Outcomes::unchanged(incoming),
                "pwd" if argv.len() == 1 => Outcomes::unchanged(incoming),
                _ => self.unknown_effect(id),
            };
        }
        Outcomes::unchanged(incoming)
    }

    fn unknown_effect(&mut self, id: BashNodeId) -> Outcomes {
        self.issue(id, BashContextIssue::UnknownStateEffect);
        Outcomes::unchanged(BashCwdSet::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        BashCommandContexts, BashContextAssumptions, BashContextIssue, BashCwdSet,
        MAX_CWD_PATH_BYTES, MAX_CWD_STATES,
    };
    use crate::bash::parse_bash;

    const INITIAL: &str = "/history/nonexistent-root";

    fn declared() -> BashContextAssumptions {
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

    fn contexts(source: &str) -> BashCommandContexts {
        parse_bash(source)
            .unwrap()
            .command_contexts_with_assumptions(Path::new(INITIAL), declared())
    }

    #[test]
    fn every_builtin_is_unknown_unless_its_state_behavior_is_explicitly_supported() {
        for builtin in super::BASH_BUILTINS {
            let result = contexts(&format!("'{builtin}'; cat note"));
            let supported = matches!(*builtin, ":" | "true" | "false" | "echo" | "pwd");
            assert_eq!(result.complete, supported, "{builtin}");
            assert_eq!(
                result.commands.last().unwrap().complete,
                supported,
                "{builtin}"
            );
        }
        for source in [
            "unregistered-cli; cat note",
            "/custom/bin/tool; cat note",
            "/usr/bin/test value; cat note",
        ] {
            let result = contexts(source);
            assert!(result.complete, "{source}");
            assert_eq!(result.commands.last().unwrap().incoming, paths(&[""]));
        }
    }

    fn paths(suffixes: &[&str]) -> BashCwdSet {
        let mut paths: Vec<PathBuf> = suffixes
            .iter()
            .map(|suffix| {
                if suffix.is_empty() {
                    PathBuf::from(INITIAL)
                } else {
                    Path::new(INITIAL).join(suffix)
                }
            })
            .collect();
        paths.sort();
        BashCwdSet::Known(paths)
    }

    #[test]
    fn a_failed_cd_does_not_change_the_base_of_the_or_branch() {
        let result = contexts("cd left || cd right; cat note.txt");
        assert!(result.complete, "{:?}", result.diagnostics);
        assert_eq!(result.commands.len(), 3);
        assert_eq!(result.commands[1].incoming, paths(&[""]));
        assert_eq!(result.commands[2].incoming, paths(&["", "left", "right"]));
        assert_eq!(result.on_success, paths(&["", "left", "right"]));
        assert_eq!(result.on_failure, paths(&["", "left", "right"]));
    }

    #[test]
    fn success_failure_and_sequence_edges_keep_all_possible_bases() {
        for (source, expected) in [
            ("cd left && cat note", paths(&["left"])),
            ("cd left || cat note", paths(&[""])),
            ("cd left; cat note", paths(&["", "left"])),
            ("cd left\ncat note", paths(&["", "left"])),
            (
                "cd left && cd right; cat note",
                paths(&["", "left", "left/right"]),
            ),
            ("cd left || cd right && cat note", paths(&["left", "right"])),
        ] {
            let result = contexts(source);
            assert!(result.complete, "{source:?}: {:?}", result.diagnostics);
            assert_eq!(
                result.commands.last().unwrap().incoming,
                expected,
                "{source:?}"
            );
        }
    }

    #[test]
    fn pipelines_subshells_and_background_cannot_leak_cwd_but_braces_can() {
        for (source, expected) in [
            ("cd left | cat note; cat outer", paths(&[""])),
            ("cat note | cd left; cat outer", paths(&[""])),
            ("cd left |& cat note; cat outer", paths(&[""])),
            ("(cd left; cat note); cat outer", paths(&[""])),
            ("cd left & cat outer", paths(&[""])),
            ("{ cd left; cat note; }; cat outer", paths(&["", "left"])),
            (
                "cd left && cat note | cat next; cat outer",
                paths(&["", "left"]),
            ),
        ] {
            let result = contexts(source);
            assert!(result.complete, "{source:?}: {:?}", result.diagnostics);
            assert_eq!(
                result.commands.last().unwrap().incoming,
                expected,
                "{source:?}"
            );
        }
        let result = contexts("cd left && cat note | cat next");
        assert_eq!(result.commands[1].incoming, paths(&["left"]));
        assert_eq!(result.commands[2].incoming, paths(&["left"]));
    }

    #[test]
    fn no_startup_or_environment_assumption_is_inferred_from_source() {
        let program = parse_bash("cd left && cat note").unwrap();
        let default = program.command_contexts(Path::new(INITIAL));
        assert!(!default.complete);
        assert!(
            default
                .commands
                .iter()
                .all(|context| !context.complete && context.incoming == BashCwdSet::Unknown)
        );
        for assumptions in [
            BashContextAssumptions {
                startup_preserves_cwd: false,
                ..declared()
            },
            BashContextAssumptions {
                no_aliases_functions_or_command_not_found_hook: false,
                ..declared()
            },
            BashContextAssumptions {
                no_traps: false,
                ..declared()
            },
            BashContextAssumptions {
                default_shell_options: false,
                ..declared()
            },
            BashContextAssumptions {
                standard_builtins: false,
                ..declared()
            },
            BashContextAssumptions {
                directory_variables_are_standard: false,
                ..declared()
            },
            BashContextAssumptions {
                cdpath_empty: false,
                ..declared()
            },
            BashContextAssumptions {
                lastpipe_disabled: false,
                ..declared()
            },
            BashContextAssumptions {
                logical_pwd_matches_initial: false,
                ..declared()
            },
        ] {
            let result = program.command_contexts_with_assumptions(Path::new(INITIAL), assumptions);
            assert!(!result.complete);
            assert_eq!(result.commands[0].incoming, BashCwdSet::Unknown);
            assert_eq!(
                result.diagnostics[0].issue,
                BashContextIssue::UndeclaredShellAssumptions
            );
        }
    }

    #[test]
    fn unsupported_state_changes_widen_following_commands_to_unknown() {
        for source in [
            "cd; cat note",
            "cd -; cat note",
            "cd -P left; cat note",
            "cd $dir; cat note",
            "CDPATH=/outside cd left; cat note",
            "CDPATH=/outside; cd left; cat note",
            "source setup; cat note",
            "eval 'cd left'; cat note",
            "pushd left; cat note",
            "read CDPATH; cd left; cat note",
            "printf -v CDPATH /outside; cat note",
            "cd left/../right; cat note",
            "test -v 'array[CDPATH=1]'; cat note",
            "wait -p CDPATH; cat note",
            "shopt -s lastpipe; cat note | cd left; cat note",
            "trap 'cd left' DEBUG; cat note",
            "if cd left; then cat a; fi; cat note",
            "f() { cd left; }; f; cat note",
        ] {
            let result = contexts(source);
            assert!(!result.complete, "{source:?}");
            assert_eq!(
                result.commands.last().unwrap().incoming,
                BashCwdSet::Unknown,
                "{source:?}"
            );
        }
    }

    #[test]
    fn history_paths_are_symbolic_and_never_canonicalized() {
        for (source, expected) in [
            ("cd missing/leaf && cat note", paths(&["missing/leaf"])),
            (
                "cd /different/nonexistent && cat note",
                BashCwdSet::Known(vec![PathBuf::from("/different/nonexistent")]),
            ),
            ("cd -- -literal && cat note", paths(&["-literal"])),
        ] {
            let result = contexts(source);
            assert!(result.complete);
            assert_eq!(result.commands.last().unwrap().incoming, expected);
        }
        let result = parse_bash("cat note")
            .unwrap()
            .command_contexts_with_assumptions(Path::new("relative"), declared());
        assert!(!result.complete);
        assert_eq!(
            result.diagnostics[0].issue,
            BashContextIssue::InvalidInitialCwd
        );
    }

    #[test]
    fn state_path_and_retained_context_budgets_widen_instead_of_dropping_branches() {
        let mut source = String::new();
        for index in 0..MAX_CWD_STATES {
            source.push_str(&format!("cd d{index}; "));
        }
        source.push_str("cat note");
        let result = contexts(&source);
        assert!(!result.complete);
        assert_eq!(
            result.commands.last().unwrap().incoming,
            BashCwdSet::Unknown
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.issue == BashContextIssue::StateLimit)
        );

        let oversized_path = format!("cd {} && cat note", "x".repeat(MAX_CWD_PATH_BYTES));
        let result = contexts(&oversized_path);
        assert!(!result.complete);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.issue == BashContextIssue::PathLimit)
        );

        let initial = format!("/{}", "x".repeat(MAX_CWD_PATH_BYTES - 1));
        let source = "cat note;".repeat(400);
        let result = parse_bash(&source)
            .unwrap()
            .command_contexts_with_assumptions(Path::new(&initial), declared());
        assert!(!result.complete);
        assert_eq!(
            result.commands.last().unwrap().incoming,
            BashCwdSet::Unknown
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.issue == BashContextIssue::RetainedBytesLimit)
        );
    }
}
