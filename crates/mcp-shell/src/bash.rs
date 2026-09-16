use std::{
    error::Error,
    fmt,
    ops::ControlFlow,
    time::{Duration, Instant},
};

use serde::Serialize;
use tree_sitter::{Node, ParseOptions, ParseState, Parser, Tree};

mod contexts;
mod lower;

pub use contexts::{
    BashCommandContext, BashCommandContexts, BashContextAssumptions, BashContextDiagnostic,
    BashContextError, BashContextIssue, BashCwdSet, MAX_CONTEXT_BYTES, MAX_CWD_PATH_BYTES,
    MAX_CWD_STATES,
};

pub const BASH_ANALYSIS_VERSION: u16 = 1;
pub const BASH_GRAMMAR_VERSION: &str = "tree-sitter-bash-0.25.1";
pub const MAX_BASH_SOURCE_BYTES: usize = 64 * 1024;
pub const MAX_BASH_CST_NODES: usize = 4096;
pub const MAX_BASH_DEPTH: usize = 64;
pub const MAX_BASH_PARSE_MILLIS: u64 = 250;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashLimits {
    pub max_source_bytes: usize,
    pub max_cst_nodes: usize,
    pub max_depth: usize,
    pub parse_millis: u64,
}

impl Default for BashLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: MAX_BASH_SOURCE_BYTES,
            max_cst_nodes: MAX_BASH_CST_NODES,
            max_depth: MAX_BASH_DEPTH,
            parse_millis: MAX_BASH_PARSE_MILLIS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashParseError {
    InvalidLimits,
    SourceLimit,
    NodeLimit,
    DepthLimit,
    ParserUnavailable,
    ParseDeadline,
}

impl fmt::Display for BashParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "Bash analysis limits must be positive and within host bounds",
            Self::SourceLimit => "Bash source exceeds the analysis byte limit",
            Self::NodeLimit => "Bash syntax exceeds the analysis node limit",
            Self::DepthLimit => "Bash syntax exceeds the analysis depth limit",
            Self::ParserUnavailable => "Bash grammar is unavailable",
            Self::ParseDeadline => "Bash parsing exceeded its deadline",
        })
    }
}

impl Error for BashParseError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashSpan {
    pub start: usize,
    pub end: usize,
}

impl BashSpan {
    fn of(node: Node<'_>) -> Self {
        Self {
            start: node.start_byte(),
            end: node.end_byte(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BashNodeId(pub usize);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum BashDiagnosticKind {
    SyntaxError,
    SourceGap,
    OverlappingCoverage,
    UnsupportedSyntax(String),
    AmbiguousRedirect,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashDiagnostic {
    pub span: BashSpan,
    pub kind: BashDiagnosticKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashOperatorKind {
    Semicolon,
    Newline,
    And,
    Or,
    Pipe,
    PipeStderr,
    Background,
    OpenSubshell,
    CloseSubshell,
    OpenBrace,
    CloseBrace,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashOperator {
    pub span: BashSpan,
    pub kind: BashOperatorKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashQuoting {
    Unquoted,
    Single,
    Double,
    AnsiC,
    Translated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum BashFragmentValue {
    Literal(String),
    Dynamic(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashWordFragment {
    pub span: BashSpan,
    pub quoting: BashQuoting,
    pub value: BashFragmentValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashWord {
    pub span: BashSpan,
    pub literal: Option<String>,
    pub fragments: Vec<BashWordFragment>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashAssignment {
    pub span: BashSpan,
    pub name: String,
    pub value: BashWord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashRedirectKind {
    Read,
    Write,
    Append,
    WriteBoth,
    AppendBoth,
    DuplicateInput,
    DuplicateOutput,
    Clobber,
    CloseInput,
    CloseOutput,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashRedirect {
    pub span: BashSpan,
    pub operator: BashSpan,
    pub kind: BashRedirectKind,
    pub descriptor: Option<BashSpan>,
    pub target: Option<BashWord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "index", rename_all = "snake_case")]
pub enum BashCommandPart {
    Word(usize),
    Assignment(usize),
    Redirect(usize),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashCommand {
    pub complete: bool,
    pub words: Vec<BashWord>,
    pub assignments: Vec<BashAssignment>,
    pub redirects: Vec<BashRedirect>,
    pub parts: Vec<BashCommandPart>,
}

impl BashCommand {
    pub fn static_argv(&self) -> Option<Vec<&str>> {
        self.complete.then(|| {
            self.words
                .iter()
                .map(|word| word.literal.as_deref())
                .collect()
        })?
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BashNodeKind {
    Sequence {
        items: Vec<BashNodeId>,
        separators: Vec<BashOperator>,
    },
    AndOr {
        left: BashNodeId,
        operator: BashOperator,
        right: BashNodeId,
    },
    Pipeline {
        commands: Vec<BashNodeId>,
        operators: Vec<BashOperator>,
    },
    Background {
        body: BashNodeId,
        operator: BashOperator,
    },
    Subshell {
        body: BashNodeId,
        open: BashOperator,
        close: BashOperator,
    },
    BraceGroup {
        body: BashNodeId,
        open: BashOperator,
        close: BashOperator,
    },
    Command {
        command: BashCommand,
    },
    Assignments {
        assignments: Vec<BashAssignment>,
    },
    Unknown {
        reason: BashDiagnosticKind,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashNode {
    pub span: BashSpan,
    pub structure: BashNodeKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "operator", rename_all = "snake_case")]
pub enum BashCoverageKind {
    Word,
    Assignment,
    Redirect,
    Operator(BashOperatorKind),
    Payload,
    Unknown,
    Trivia,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashCoverage {
    pub span: BashSpan,
    pub owner: BashNodeId,
    pub role: BashCoverageKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashProgram {
    analysis_version: u16,
    grammar_version: &'static str,
    source: String,
    root: BashNodeId,
    nodes: Vec<BashNode>,
    coverage: Vec<BashCoverage>,
    diagnostics: Vec<BashDiagnostic>,
}

impl BashProgram {
    pub fn source(&self) -> &str {
        &self.source
    }
    pub const fn analysis_version(&self) -> u16 {
        self.analysis_version
    }
    pub const fn grammar_version(&self) -> &'static str {
        self.grammar_version
    }
    pub const fn root(&self) -> BashNodeId {
        self.root
    }
    pub fn nodes(&self) -> &[BashNode] {
        &self.nodes
    }
    pub fn coverage(&self) -> &[BashCoverage] {
        &self.coverage
    }
    pub fn diagnostics(&self) -> &[BashDiagnostic] {
        &self.diagnostics
    }
    pub fn is_complete(&self) -> bool {
        self.diagnostics.is_empty()
    }
    pub fn text(&self, span: &BashSpan) -> Option<&str> {
        self.source.get(span.start..span.end)
    }

    pub fn commands(&self) -> impl Iterator<Item = (BashNodeId, &BashCommand)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match &node.structure {
                BashNodeKind::Command { command } => Some((BashNodeId(index), command)),
                _ => None,
            })
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let node_bytes = self.nodes.capacity().saturating_mul(size_of::<BashNode>());
        let coverage_bytes = self
            .coverage
            .capacity()
            .saturating_mul(size_of::<BashCoverage>());
        let diagnostic_bytes = self
            .diagnostics
            .capacity()
            .saturating_mul(size_of::<BashDiagnostic>());
        node_bytes
            .saturating_add(coverage_bytes)
            .saturating_add(diagnostic_bytes)
            .saturating_add(self.source.capacity())
            .saturating_add(
                self.nodes
                    .iter()
                    .map(lower::retained_node_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                self.diagnostics
                    .iter()
                    .map(|diagnostic| lower::retained_diagnostic_bytes(&diagnostic.kind))
                    .fold(0, usize::saturating_add),
            )
    }
}

pub fn parse_bash(source: &str) -> Result<BashProgram, BashParseError> {
    parse_bash_with_limits(source, &BashLimits::default())
}

pub fn parse_bash_with_limits(
    source: &str,
    limits: &BashLimits,
) -> Result<BashProgram, BashParseError> {
    let tree = parse_tree(source, limits)?;
    Ok(lower_tree(source, &tree))
}

pub(crate) fn parse_tree(source: &str, limits: &BashLimits) -> Result<Tree, BashParseError> {
    let started = Instant::now();
    parse_tree_with_clock(source, limits, || started.elapsed())
}

fn parse_tree_with_clock(
    source: &str,
    limits: &BashLimits,
    mut elapsed: impl FnMut() -> Duration,
) -> Result<Tree, BashParseError> {
    if limits.max_source_bytes == 0
        || limits.max_source_bytes > MAX_BASH_SOURCE_BYTES
        || limits.max_cst_nodes == 0
        || limits.max_cst_nodes > MAX_BASH_CST_NODES
        || limits.max_depth == 0
        || limits.max_depth > MAX_BASH_DEPTH
        || limits.parse_millis == 0
        || limits.parse_millis > MAX_BASH_PARSE_MILLIS
    {
        return Err(BashParseError::InvalidLimits);
    }
    if source.len() > limits.max_source_bytes {
        return Err(BashParseError::SourceLimit);
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|_| BashParseError::ParserUnavailable)?;
    let mut progress = |_: &ParseState| {
        if elapsed() >= Duration::from_millis(limits.parse_millis) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut read = |offset: usize, _| &source.as_bytes()[offset..];
    let tree = parser
        .parse_with_options(
            &mut read,
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
        .ok_or(BashParseError::ParseDeadline)?;
    let mut cursor = tree.walk();
    let mut depth = 0;
    let mut visited = 0;
    loop {
        visited += 1;
        if visited > limits.max_cst_nodes {
            return Err(BashParseError::NodeLimit);
        }
        if depth > limits.max_depth {
            return Err(BashParseError::DepthLimit);
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                drop(cursor);
                return Ok(tree);
            }
            depth -= 1;
        }
    }
}

pub(crate) fn lower_tree(source: &str, tree: &Tree) -> BashProgram {
    lower::lower(source, tree.root_node())
}

pub(crate) use lower::decode_static_word;

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use serde_json::json;

    use super::{
        BASH_ANALYSIS_VERSION, BASH_GRAMMAR_VERSION, BashCommand, BashCoverageKind,
        BashDiagnosticKind, BashLimits, BashNodeKind, BashOperatorKind, BashParseError,
        BashProgram, BashQuoting, BashRedirectKind, MAX_BASH_SOURCE_BYTES, parse_bash,
        parse_bash_with_limits, parse_tree_with_clock,
    };
    use crate::{ShellInput, ShellPermissionPolicy, ShellToolGroup, ShellWord};

    const HEREDOC: &str = "python3 - <<'PY'\nprint('one')\nPY\n";
    const REDIRECT_TAILS: &str = "cat first >out second 2>&1 third";
    const DEADLINE_FIXTURE_REPETITIONS: usize = 256;
    const COVERAGE_FIXTURES: &[&str] = &[
        "",
        " \t\n# only a comment\n",
        "pwd",
        "cat '' \"\" a\"b\"'c'",
        "cat '&&' '|' ';' escaped\\ word",
        "git\\\n status && rg needle src | wc -l",
        "a; b\nc && d || e |& f & g",
        "(cd left; cat note); cat outer",
        "{ cd left && cat note; }; cat outer",
        "MODE= X='a b' tool one",
        "A= B=''",
        ">out cat first",
        REDIRECT_TAILS,
        "cat <input extra >>log last",
        "cat 3<&-",
        "cat 2>&-",
        "cat &>all",
        "cat >|output",
        "cat >\"$target\"",
        "cat $HOME ~ *.rs pre\"$NAME\"post",
        "echo λ '日本語'",
        HEREDOC,
        "python3 - <<'PY' && cat tail\nprint('one')\nPY\n",
        "python3 - <<'PY' || cat tail\nprint('one')\nPY\n",
        "python3 - <<'PY' | cat tail\nprint('one')\nPY\n",
        "cat <<EOF\n$(touch never)\nEOF\n",
        "cat <<A <<B\none\nA\ntwo\nB\n",
        "cat <<<payload",
        "if cat a; then cat b; fi",
        "for x in a b; do cat x; done",
        "f() { cd elsewhere; }; f",
        "cat $(cd elsewhere)",
        "cat <(pwd)",
        "[ -f note ] && cat note",
        "[[ -f note ]] && cat note",
        "echo 'unterminated",
        "a &&",
        "cat >",
        "echo a\r\necho b",
        "echo a\0b",
    ];

    #[test]
    fn the_real_parser_progress_callback_obeys_an_injected_deadline_without_sleeping() {
        let source = "cat note;\n".repeat(DEADLINE_FIXTURE_REPETITIONS);
        let limits = BashLimits::default();
        let deadline = Duration::from_millis(limits.parse_millis);
        let mut calls = 0;
        let result = parse_tree_with_clock(&source, &limits, || {
            calls += 1;
            deadline
        });
        assert!(calls > 0);
        assert_eq!(result.err(), Some(BashParseError::ParseDeadline));

        let mut calls = 0;
        let result = parse_tree_with_clock(&source, &limits, || {
            calls += 1;
            Duration::ZERO
        });
        assert!(calls > 0);
        assert!(result.is_ok());
    }

    fn assert_coverage(program: &BashProgram) {
        let mut offset = 0;
        for covered in program.coverage() {
            assert_eq!(covered.span.start, offset, "{:?}", program.source());
            assert!(covered.span.end > covered.span.start);
            assert!(covered.owner.0 < program.nodes().len());
            assert!(program.text(&covered.span).is_some());
            offset = covered.span.end;
        }
        assert_eq!(offset, program.source().len(), "{:?}", program.source());
    }

    fn first_command(program: &BashProgram) -> &BashCommand {
        program.commands().next().expect("represented command").1
    }

    #[test]
    fn every_source_byte_has_exactly_one_ledger_owner_including_unknown_payloads() {
        for source in COVERAGE_FIXTURES {
            let program = parse_bash(source).unwrap();
            assert_coverage(&program);
        }
    }

    #[test]
    fn quoting_keeps_empty_words_operators_and_joined_fragments_as_data() {
        let program = parse_bash("tool '' \"\" '&&' '|' ';' a\"b\"'c' escaped\\ word").unwrap();
        assert!(program.is_complete(), "{:?}", program.diagnostics());
        let command = first_command(&program);
        assert_eq!(
            command.static_argv().unwrap(),
            ["tool", "", "", "&&", "|", ";", "abc", "escaped word"]
        );
        assert_eq!(command.words[6].fragments.len(), 3);
        assert_eq!(command.words[6].fragments[1].quoting, BashQuoting::Double);
        assert!(
            !program
                .coverage()
                .iter()
                .any(|covered| matches!(covered.role, BashCoverageKind::Operator(_)))
        );
    }

    #[test]
    fn expansions_are_not_promoted_to_static_argv() {
        for source in [
            "cat $HOME",
            "cat ~",
            "cat *.rs",
            "cat a\"$NAME\"b",
            "cat {a,b}",
            "cat $'escaped'",
            "cat $\"translated\"",
        ] {
            let program = parse_bash(source).unwrap();
            assert!(
                first_command(&program).static_argv().is_none(),
                "{source:?}"
            );
            assert!(
                first_command(&program).words[1].literal.is_none(),
                "{source:?}"
            );
        }
    }

    #[test]
    fn operators_preserve_left_associativity_and_pipeline_precedence() {
        let program = parse_bash("a || b && c | d; e\nf & g").unwrap();
        assert!(program.is_complete(), "{:?}", program.diagnostics());
        let BashNodeKind::Sequence { items, separators } =
            &program.nodes()[program.root().0].structure
        else {
            panic!("sequence");
        };
        assert_eq!(items.len(), 4);
        assert_eq!(
            separators
                .iter()
                .map(|operator| &operator.kind)
                .collect::<Vec<_>>(),
            [
                &BashOperatorKind::Semicolon,
                &BashOperatorKind::Newline,
                &BashOperatorKind::Background
            ]
        );
        let BashNodeKind::AndOr {
            left,
            operator,
            right,
        } = &program.nodes()[items[0].0].structure
        else {
            panic!("and/or");
        };
        assert_eq!(operator.kind, BashOperatorKind::And);
        assert!(
            matches!(&program.nodes()[left.0].structure, BashNodeKind::AndOr { operator, .. } if operator.kind == BashOperatorKind::Or)
        );
        assert!(matches!(
            program.nodes()[right.0].structure,
            BashNodeKind::Pipeline { .. }
        ));
        assert!(matches!(
            program.nodes()[items[2].0].structure,
            BashNodeKind::Background { .. }
        ));
    }

    #[test]
    fn redirect_destinations_and_absorbed_tail_arguments_keep_their_order() {
        let program = parse_bash(REDIRECT_TAILS).unwrap();
        assert!(program.is_complete(), "{:?}", program.diagnostics());
        let command = first_command(&program);
        assert_eq!(
            command.static_argv().unwrap(),
            ["cat", "first", "second", "third"]
        );
        assert_eq!(command.redirects.len(), 2);
        assert_eq!(command.redirects[0].kind, BashRedirectKind::Write);
        assert_eq!(command.redirects[1].kind, BashRedirectKind::DuplicateOutput);
        assert_eq!(program.text(&command.redirects[0].span), Some(">out"));
        assert_eq!(program.text(&command.redirects[1].span), Some("2>&1"));
        assert_eq!(
            command.redirects[0]
                .target
                .as_ref()
                .unwrap()
                .literal
                .as_deref(),
            Some("out")
        );
        let reversed = parse_bash("cat first 2>&1 >out").unwrap();
        assert_eq!(
            first_command(&reversed).redirects[0].kind,
            BashRedirectKind::DuplicateOutput
        );
        assert_eq!(
            first_command(&reversed).redirects[1].kind,
            BashRedirectKind::Write
        );
    }

    #[test]
    fn assignments_and_prefix_redirects_are_separate_from_argv() {
        let program = parse_bash("A= B='a b' >out tool '' tail").unwrap();
        assert!(program.is_complete(), "{:?}", program.diagnostics());
        let command = first_command(&program);
        assert_eq!(command.static_argv().unwrap(), ["tool", "", "tail"]);
        assert_eq!(command.assignments.len(), 2);
        assert_eq!(command.assignments[0].value.literal.as_deref(), Some(""));
        assert_eq!(command.assignments[1].value.literal.as_deref(), Some("a b"));
        assert_eq!(command.redirects.len(), 1);
    }

    #[test]
    fn the_real_pre_heredoc_argument_gap_refuses_an_incomplete_python_argv() {
        let program = parse_bash(HEREDOC).unwrap();
        assert!(!program.is_complete());
        assert!(program.commands().next().is_none());
        assert!(
            program
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.kind == BashDiagnosticKind::SourceGap)
        );
        assert!(
            program
                .coverage()
                .iter()
                .any(|covered| covered.role == BashCoverageKind::Payload
                    && program.text(&covered.span) == Some("print('one')\n"))
        );
        assert_coverage(&program);
    }

    #[test]
    fn heredoc_owned_control_tails_and_payloads_cannot_disappear() {
        for source in [
            "python3 <<'PY' && cat tail\nprint('one')\nPY\n",
            "python3 <<'PY' || cat tail\nprint('one')\nPY\n",
            "python3 <<'PY' | cat tail\nprint('one')\nPY\n",
            "python3 <<'PY' >out arg\nprint('one')\nPY\n",
        ] {
            let program = parse_bash(source).unwrap();
            assert!(!program.is_complete(), "{source:?}");
            assert!(
                program
                    .coverage()
                    .iter()
                    .any(|covered| covered.role == BashCoverageKind::Payload)
            );
            assert!(program.coverage().iter().any(|covered| covered.role
                == BashCoverageKind::Unknown
                && program.text(&covered.span).unwrap().contains("tail")
                || covered.role == BashCoverageKind::Unknown
                    && program.text(&covered.span).unwrap().contains("arg")));
            assert_coverage(&program);
        }
        let other = parse_bash(&HEREDOC.replace("one", "two")).unwrap();
        assert_ne!(parse_bash(HEREDOC).unwrap(), other);
    }

    #[test]
    fn unsupported_and_deferred_execution_is_unknown_not_flattened_commands() {
        for source in [
            "if cat a; then cat b; fi",
            "for x in a; do cat x; done",
            "while cat a; do cat b; done",
            "f() { cat secret; }; f",
            "cat $(cat secret)",
            "cat `cat secret`",
            "cat <(cat secret)",
            "cat $((x=1))",
            "[[ -f note ]]",
            "[ -f note ]",
            "cat <<<payload",
            "A=(a b)",
        ] {
            let program = parse_bash(source).unwrap();
            assert!(!program.is_complete(), "{source:?}");
            assert!(
                program
                    .nodes()
                    .iter()
                    .any(|node| matches!(node.structure, BashNodeKind::Unknown { .. }))
            );
            assert!(
                program
                    .commands()
                    .all(|(_, command)| !command.complete && command.static_argv().is_none())
            );
            assert_coverage(&program);
        }
    }

    #[test]
    fn malformed_sources_never_produce_a_complete_program() {
        for source in ["echo 'unterminated", "a &&", "cat >", "if a; then b"] {
            let program = parse_bash(source).unwrap();
            assert!(!program.is_complete(), "{source:?}");
            assert!(
                program
                    .diagnostics()
                    .iter()
                    .any(|diagnostic| diagnostic.kind == BashDiagnosticKind::SyntaxError)
            );
        }
    }

    #[test]
    fn source_node_and_depth_limits_are_typed_and_cannot_be_relaxed() {
        assert_eq!(
            parse_bash(&"a".repeat(MAX_BASH_SOURCE_BYTES + 1)),
            Err(BashParseError::SourceLimit)
        );
        for (limits, expected) in [
            (
                BashLimits {
                    max_source_bytes: 1,
                    ..BashLimits::default()
                },
                BashParseError::SourceLimit,
            ),
            (
                BashLimits {
                    max_cst_nodes: 1,
                    ..BashLimits::default()
                },
                BashParseError::NodeLimit,
            ),
            (
                BashLimits {
                    max_depth: 1,
                    ..BashLimits::default()
                },
                BashParseError::DepthLimit,
            ),
            (
                BashLimits {
                    max_source_bytes: MAX_BASH_SOURCE_BYTES + 1,
                    ..BashLimits::default()
                },
                BashParseError::InvalidLimits,
            ),
            (
                BashLimits {
                    parse_millis: 0,
                    ..BashLimits::default()
                },
                BashParseError::InvalidLimits,
            ),
        ] {
            assert_eq!(parse_bash_with_limits("cat x", &limits), Err(expected));
        }
    }

    #[test]
    fn json_has_versioned_structure_not_an_implicit_opaque_completeness_claim() {
        let program = parse_bash("cat ''").unwrap();
        let json = serde_json::to_value(&program).unwrap();
        assert_eq!(json["analysis_version"], BASH_ANALYSIS_VERSION);
        assert_eq!(json["grammar_version"], BASH_GRAMMAR_VERSION);
        assert_eq!(json["nodes"][1]["structure"]["kind"], "command");
        assert_eq!(
            json["nodes"][1]["structure"]["command"]["words"][1]["literal"],
            ""
        );
        assert_eq!(
            serde_json::to_value(BashParseError::NodeLimit).unwrap(),
            json!("node_limit")
        );
        assert!(
            !program
                .command_contexts(Path::new("/history/nonexistent"))
                .complete
        );
    }

    #[tokio::test]
    async fn native_preparation_and_history_parsing_expose_identical_programs_and_corrected_scopes()
    {
        let directory = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(directory.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        for source in [
            REDIRECT_TAILS,
            HEREDOC,
            "cd left || cd right; cat note.txt",
            "cat ''",
        ] {
            let prepared = group
                .prepare(ShellInput {
                    command: source.to_owned(),
                    timeout: None,
                    workdir: None,
                })
                .await
                .unwrap();
            assert_eq!(
                prepared.bash_program().unwrap(),
                &parse_bash(source).unwrap()
            );
            assert!(prepared.retained_bytes() >= prepared.bash_program().unwrap().retained_bytes());
            if source == REDIRECT_TAILS {
                assert_eq!(
                    prepared.analysis().scopes[0].arguments,
                    Some(vec![
                        ShellWord::Literal("first".into()),
                        ShellWord::Literal("second".into()),
                        ShellWord::Literal("third".into())
                    ])
                );
            }
            if source == HEREDOC {
                assert!(prepared.analysis().opaque);
                assert!(prepared.analysis().scopes[0].arguments.is_none());
            }
            if source.starts_with("cd ") {
                assert!(!prepared.analysis().opaque);
                assert!(
                    !prepared
                        .bash_program()
                        .unwrap()
                        .command_contexts(prepared.workdir())
                        .complete
                );
            }
        }
        assert_eq!(directory.path().read_dir().unwrap().count(), 0);
    }
}
