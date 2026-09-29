//! Recognizes a shell command that does nothing a native tool could not.
//!
//! The habit this answers is routing: a model trained on shell-first agents
//! reaches for `rg` or `cat` even where a structured, bounded tool is declared
//! beside it, and no amount of prompt prose moves it. A refusal naming the tool
//! to call does.
//!
//! Recognition is deliberately narrow. Every word of the line has to be
//! accounted for, and every flag has to have a native equivalent, so the moment
//! a command asks for something the native tool cannot express the line is left
//! alone. The failure this avoids is far worse than the one it prevents:
//! refusing a search that no tool can then perform leaves a model with nowhere
//! to go.

use workcell::shell::{
    ShellCommandScope,
    bash::{BashCommand, BashFragmentValue, BashNodeKind, BashOperatorKind, BashProgram},
};

use crate::pattern_analysis::CommandFacts;
use crate::read_only_shell::literal_arguments;

const END_OF_FLAGS: &str = "--";
const GREP: Native = Native::new("file_grep", "pattern, path, include, -A/-B/-C, head_limit");
const READ: Native = Native::new("file_read", "filePath, offset, limit");
const GLOB: Native = Native::new("file_glob", "pattern, path");
pub(crate) const WORKDIR_REFUSAL: &str = "Use the shell tool's workdir argument instead of a leading \
    cd. Remove the leading cd <directory> && from command and set workdir to that directory. \
    Resolve a relative directory against this call's initial workdir (the project directory when \
    omitted), not against a different base. Keep the remaining command unchanged. \
    Set agent.shell_workdir_redirect = false in user config to disable this check.";

pub(crate) fn leading_workdir(program: &BashProgram) -> bool {
    if !program.is_complete()
        || !program.nodes().iter().all(|node| match &node.structure {
            BashNodeKind::Sequence { items, .. } => items.len() == 1,
            BashNodeKind::AndOr { operator, .. } => operator.kind == BashOperatorKind::And,
            BashNodeKind::Command { .. } | BashNodeKind::Pipeline { .. } => true,
            _ => false,
        })
    {
        return false;
    }
    let mut node = program.root();
    let mut conditional = false;
    loop {
        match &program.nodes()[node.0].structure {
            BashNodeKind::Sequence { items, .. } => node = items[0],
            BashNodeKind::AndOr { left, .. } => {
                conditional = true;
                node = *left;
            }
            BashNodeKind::Command { command }
                if conditional
                    && command.assignments.is_empty()
                    && command.redirects.is_empty() =>
            {
                let Some(argv) = command.static_argv() else {
                    return false;
                };
                if !matches!(argv.as_slice(), ["cd", path] | ["cd", "--", path]
                    if !path.is_empty() && !path.starts_with('-'))
                {
                    return false;
                }
                return program
                    .commands()
                    .all(|(id, command)| id == node || !uses_directory_state(program, command));
            }
            _ => return false,
        }
    }
}

fn uses_directory_state(program: &BashProgram, command: &BashCommand) -> bool {
    command
        .words
        .first()
        .is_some_and(|word| matches!(word.literal.as_deref(), Some("cd" | "command" | "builtin")))
        || command
            .words
            .iter()
            .chain(
                command
                    .assignments
                    .iter()
                    .map(|assignment| &assignment.value),
            )
            .chain(
                command
                    .redirects
                    .iter()
                    .filter_map(|redirect| redirect.target.as_ref()),
            )
            .any(|word| {
                word.fragments.iter().any(|fragment| {
                    matches!(fragment.value, BashFragmentValue::Dynamic(_))
                        && program
                            .text(&fragment.span)
                            .is_some_and(|source| source.contains("PWD"))
                })
            })
}

/// The tool to call instead, and the parameters that make the call without a
/// second lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Native {
    pub(crate) name: &'static str,
    signature: &'static str,
}

impl Native {
    const fn new(name: &'static str, signature: &'static str) -> Self {
        Self { name, signature }
    }
}

/// What one command in a line contributes.
enum Role {
    /// A native tool covers it completely, and this is the call to make.
    Duplicate(Native),
    /// It produces nothing of its own, so it neither saves nor blocks the line.
    Glue,
    /// It does something no native tool does, which settles the whole line.
    Distinct,
}

/// The tools that could have answered the whole line, or `None` when any part
/// of it needs a shell.
///
/// A redirection settles the line whatever the command reads: `cat notes.txt`
/// is a whole-file read, and `cat notes.txt > copy.txt` is a copy no read tool
/// performs. The parse is what distinguishes them, because the grammar hangs
/// `> copy.txt` off a node above the command and the scope reads the same
/// either way.
pub(crate) fn detect(commands: &[CommandFacts<'_>]) -> Option<Vec<Native>> {
    let mut natives: Vec<Native> = Vec::new();
    for facts in commands {
        if !facts.command.redirects.is_empty() {
            return None;
        }
        match role(&facts.scope) {
            Role::Distinct => return None,
            Role::Glue => {}
            Role::Duplicate(native) if !natives.contains(&native) => natives.push(native),
            Role::Duplicate(_) => {}
        }
    }
    (!natives.is_empty()).then_some(natives)
}

/// Names the call to make, because a refusal that only says no leaves the model
/// to guess and it guesses the shell again.
pub(crate) fn refusal(natives: &[Native]) -> String {
    let tools = natives
        .iter()
        .map(|native| format!("{} ({})", native.name, native.signature))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "This command only re-does what {tools} already does, and the tool returns a \
         bounded, structured result the shell cannot. Call it instead. Run the command \
         through the shell when you need something the tool has no equivalent for, such \
         as a case-insensitive or inverted search, or when its output feeds a genuine \
         pipeline."
    )
}

fn role(scope: &ShellCommandScope) -> Role {
    // A spelled-out path is a different program from the basename the rules
    // below describe, and a word that does not mean its own text could be
    // anything at all.
    if scope.source != scope.normalized {
        return Role::Distinct;
    }
    let Some(arguments) = literal_arguments(scope) else {
        return Role::Distinct;
    };
    match scope.executable.as_str() {
        "rg" | "grep" => search_role(&arguments),
        "cat" => cat_role(&arguments),
        "head" => head_role(&arguments),
        "find" => find_role(&arguments),
        "cd" | "echo" | "true" | ":" => Role::Glue,
        _ => Role::Distinct,
    }
}

fn search_role(arguments: &[&str]) -> Role {
    let mut words = arguments.iter();
    while let Some(argument) = words.next() {
        if *argument == END_OF_FLAGS {
            break;
        }
        if !argument.starts_with('-') {
            continue;
        }
        match search_flag(argument) {
            Some(true) if words.next().is_none() => return Role::Distinct,
            Some(_) => {}
            None => return Role::Distinct,
        }
    }
    Role::Duplicate(GREP)
}

/// `Some(true)` when the flag also consumes the following word, `None` when the
/// native search cannot express it.
///
/// A cluster such as `-ni` is rejected by construction: only the first letter is
/// read, and an unconsumed remainder means the word held more than this flag.
fn search_flag(argument: &str) -> Option<bool> {
    if let Some(rest) = argument.strip_prefix("--") {
        let (name, attached) = match rest.split_once('=') {
            Some((name, _)) => (name, true),
            None => (rest, false),
        };
        return match name {
            "line-number" | "with-filename" | "no-heading" => Some(false),
            "after-context" | "before-context" | "context" | "glob" | "include" | "color" => {
                Some(!attached)
            }
            _ => None,
        };
    }
    let rest = argument.strip_prefix('-')?;
    // `grep -5` is three lines of context on either side.
    if !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(false);
    }
    let (letter, attached) = rest.split_at_checked(1)?;
    match letter {
        "n" | "H" => attached.is_empty().then_some(false),
        "A" | "B" | "C" | "g" => Some(attached.is_empty()),
        _ => None,
    }
}

/// Reading stdin is a pass-through that adds nothing, which is the one shape of
/// `cat` worth keeping: it lets `cat notes.txt` be recognized without also
/// blocking a pipeline that happens to end in one.
fn cat_role(arguments: &[&str]) -> Role {
    let mut operands = 0usize;
    for argument in arguments {
        if *argument == END_OF_FLAGS {
            continue;
        }
        if argument.starts_with('-') && argument.len() > 1 {
            return Role::Distinct;
        }
        operands += 1;
    }
    if operands == 0 {
        Role::Glue
    } else {
        Role::Duplicate(READ)
    }
}

fn head_role(arguments: &[&str]) -> Role {
    let mut words = arguments.iter();
    let mut operands = 0usize;
    while let Some(argument) = words.next() {
        if *argument == END_OF_FLAGS {
            continue;
        }
        let Some(rest) = argument.strip_prefix('-').filter(|rest| !rest.is_empty()) else {
            operands += 1;
            continue;
        };
        match rest {
            "n" if words.next().is_some() => {}
            "-lines" => {
                if words.next().is_none() {
                    return Role::Distinct;
                }
            }
            _ if rest.bytes().all(|byte| byte.is_ascii_digit()) => {}
            _ => match rest
                .strip_prefix("n")
                .or_else(|| rest.strip_prefix("-lines="))
            {
                Some(count) if count.bytes().all(|byte| byte.is_ascii_digit()) => {}
                _ => return Role::Distinct,
            },
        }
    }
    if operands == 0 {
        Role::Glue
    } else {
        Role::Duplicate(READ)
    }
}

/// `find` primaries are words, not flags, so anything unrecognized settles the
/// line rather than being read as an operand. `-exec` and `-delete` reach the
/// rest of the shell and must never be recognized here.
fn find_role(arguments: &[&str]) -> Role {
    let mut words = arguments.iter();
    while let Some(argument) = words.next() {
        if !argument.starts_with('-') {
            continue;
        }
        match *argument {
            "-name" | "-maxdepth" if words.next().is_some() => {}
            "-type" if matches!(words.next(), Some(&("f" | "d"))) => {}
            _ => return Role::Distinct,
        }
    }
    Role::Duplicate(GLOB)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use test_case::test_case;
    use workcell::shell::bash::parse_bash;

    use super::{detect, leading_workdir, refusal as message};
    use crate::pattern_analysis::shell_facts;

    const PROJECT: &str = "/home/dev/project";

    #[test_case("cd ../workcell-mcp && grep -rn --include=*.rs -E"; "user_example")]
    #[test_case("cd src && cargo test"; "shell_command")]
    #[test_case("cd /tmp/project && cargo test"; "absolute_path")]
    #[test_case("cd 'path with spaces' && cargo test"; "single_quoted_path")]
    #[test_case("cd \"path with spaces\" && cargo test"; "double_quoted_path")]
    #[test_case("cd path\\ with\\ spaces && cargo test"; "escaped_path")]
    #[test_case("  \ncd -- src && cargo test\n"; "whitespace_and_end_of_flags")]
    #[test_case("cd src && cargo build && cargo test"; "and_chain")]
    #[test_case("cd src && rg -i needle | sort"; "remaining_pipeline")]
    #[test_case("cd src && printf '%s' \"$VALUE\""; "remaining_expansion")]
    fn leading_cd_redirects_to_workdir(command: &str) {
        let program = parse_bash(command).expect("program");
        assert!(leading_workdir(&program), "{program:?}");
    }

    #[test_case("cargo test"; "no_cd")]
    #[test_case("cd src"; "standalone_cd")]
    #[test_case("echo 'cd src && cargo test'"; "quoted_command")]
    #[test_case("true && cd src && cargo test"; "non_leading_cd")]
    #[test_case("cd src; cargo test"; "semicolon")]
    #[test_case("cd src\ncargo test"; "newline")]
    #[test_case("cd src && cargo test; pwd"; "trailing_sequence")]
    #[test_case("cd src && cargo test > output.txt"; "redirected_list")]
    #[test_case("cd src || cargo test"; "fallback")]
    #[test_case("cd src && cargo test || pwd"; "trailing_fallback")]
    #[test_case("(cd src && cargo test)"; "subshell")]
    #[test_case("{ cd src && cargo test; }"; "brace_group")]
    #[test_case("cd src && cargo test &"; "background")]
    #[test_case("cd src | cat && cargo test"; "piped_cd")]
    #[test_case("CDPATH=/tmp cd src && cargo test"; "assignment")]
    #[test_case("cd src 2>/dev/null && cargo test"; "redirected_cd")]
    #[test_case("command cd src && cargo test"; "wrapped_cd")]
    #[test_case("./cd src && cargo test"; "executable_path")]
    #[test_case("cd \"$DIR\" && cargo test"; "dynamic_path")]
    #[test_case("cd $(pwd) && cargo test"; "command_substitution")]
    #[test_case("cd ~/src && cargo test"; "tilde")]
    #[test_case("cd src/* && cargo test"; "glob")]
    #[test_case("cd && cargo test"; "home_directory")]
    #[test_case("cd - && cargo test"; "previous_directory")]
    #[test_case("cd -P src && cargo test"; "physical_directory")]
    #[test_case("cd -L src && cargo test"; "logical_directory_flag")]
    #[test_case("cd '' && cargo test"; "empty_path")]
    #[test_case("cd src extra && cargo test"; "extra_argument")]
    #[test_case("cd src &&"; "incomplete_command")]
    #[test_case("cd src && cd - && cargo test"; "previous_directory_in_suffix")]
    #[test_case("cd link && cd .. && cargo test"; "logical_parent_in_suffix")]
    #[test_case("cd src && cd nested && cargo test"; "multiple_directory_changes")]
    #[test_case("cd src && builtin cd - && cargo test"; "wrapped_directory_change")]
    #[test_case("cd src && printf '%s' \"$OLDPWD\""; "previous_directory_expansion")]
    #[test_case("cd src && printf '%s' \"${OLDPWD}\""; "braced_previous_directory")]
    #[test_case("cd link && printf '%s' \"$PWD\""; "logical_directory_expansion")]
    #[test_case("cd src && PREVIOUS=$OLDPWD cargo test"; "previous_directory_assignment")]
    #[test_case("cd src && cargo test > \"$OLDPWD/output\""; "previous_directory_redirect")]
    fn non_replaceable_cd_is_left_alone(command: &str) {
        let program = parse_bash(command).expect("program");
        assert!(!leading_workdir(&program), "{program:?}");
    }

    /// Runs the real parse, so quoting, pipelines, and redirections reach the
    /// rules exactly as they do in a session.
    fn refusal(command: &str) -> Option<Vec<&'static str>> {
        let program = parse_bash(command).expect("program");
        let contexts = program.command_contexts(Path::new(PROJECT));
        let facts = shell_facts(&program, &contexts);
        detect(&facts.commands).map(|natives| natives.iter().map(|native| native.name).collect())
    }

    #[test_case("rg needle"; "a bare search")]
    #[test_case("rg -n needle src"; "a search asking for line numbers it already gets")]
    #[test_case("rg -A 5 needle"; "a search for trailing context")]
    #[test_case("rg -C3 needle"; "an attached context count")]
    #[test_case("grep -5 needle notes.txt"; "the numeric context shorthand")]
    #[test_case("rg --glob '*.rs' needle"; "a search filtered by glob")]
    #[test_case("rg --color=never needle"; "a search turning colour off")]
    #[test_case("cat notes.txt"; "a whole file read")]
    #[test_case("cat a.txt b.txt"; "several whole file reads")]
    #[test_case("head -20 notes.txt"; "a bounded read from the top")]
    #[test_case("head -n 20 notes.txt"; "a bounded read spelled with a count flag")]
    #[test_case("find . -name '*.rs'"; "a search for files by name")]
    #[test_case("find src -type f -maxdepth 2"; "a bounded listing")]
    #[test_case("rg alpha; echo ---; rg beta"; "a chain of searches glued by echo")]
    #[test_case("cd src && rg needle"; "a search reached by changing directory")]
    #[test_case("cat notes.txt | rg needle"; "a file poured into a search")]
    #[test_case("rg needle | head -20"; "a search bounded by head")]
    fn a_line_a_native_tool_covers_is_refused(command: &str) {
        assert!(
            refusal(command).is_some(),
            "{command} should have been redirected to a native tool"
        );
    }

    #[test_case("rg -i needle"; "a case-insensitive search")]
    #[test_case("rg -l needle"; "a search for names alone")]
    #[test_case("rg -c needle"; "a count of matches")]
    #[test_case("rg -o needle"; "a search printing only the match")]
    #[test_case("rg -v needle"; "an inverted search")]
    #[test_case("rg --json needle"; "a search asking for json")]
    #[test_case("rg -ni needle"; "an unsupported flag hidden in a cluster")]
    #[test_case("rg --hidden needle"; "a search of files the native tool skips")]
    #[test_case("rg -t rust needle"; "a search filtered by file type")]
    #[test_case("rg --no-ignore needle"; "a search ignoring the ignore files")]
    #[test_case("cat -n notes.txt"; "a read that numbers its lines")]
    #[test_case("head -c 40 notes.txt"; "a read bounded by bytes")]
    #[test_case("find . -exec rm {} ';'"; "a find that runs a command")]
    #[test_case("find . -newer notes.txt"; "a find with an unrecognized primary")]
    #[test_case("find . -delete"; "a find that removes what it matches")]
    #[test_case("rg needle | awk '{print $1}'"; "a search feeding a pipeline")]
    #[test_case("cat notes.txt | wc -l"; "a file counted by another program")]
    #[test_case("cargo test"; "a command no tool replaces")]
    #[test_case("echo hello"; "glue with nothing to redirect")]
    #[test_case("cd src"; "glue that only moves")]
    #[test_case("rg needle > out.txt"; "a search captured to a file")]
    #[test_case("cat notes.txt > copy.txt"; "a read that is really a copy")]
    #[test_case("cat > notes.txt <<'EOF'\nhi\nEOF\n"; "a heredoc writing a whole file")]
    #[test_case("rg \"$PATTERN\" src"; "a pattern the shell will expand")]
    fn a_line_needing_a_shell_is_left_alone(command: &str) {
        assert_eq!(
            refusal(command),
            None,
            "{command} should have been left to the shell"
        );
    }

    #[test]
    fn every_tool_the_line_could_have_used_reaches_the_refusal() {
        let natives = refusal("cat notes.txt; rg needle; find . -name '*.rs'")
            .expect("a line of pure duplicates is refused");

        assert_eq!(natives, ["file_read", "file_grep", "file_glob"]);
    }

    #[test]
    fn the_refusal_hands_back_the_parameters_the_call_needs() {
        let natives = refusal("rg -A 5 needle").expect("a search is refused");
        let refusal = message(&[super::GREP]);

        assert_eq!(natives, ["file_grep"]);
        for parameter in ["pattern", "path", "include", "-A/-B/-C", "head_limit"] {
            assert!(
                refusal.contains(parameter),
                "{refusal:?} must name {parameter}"
            );
        }
    }
}
