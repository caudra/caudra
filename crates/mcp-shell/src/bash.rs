use std::{
    error::Error,
    fmt,
    ops::ControlFlow,
    time::{Duration, Instant},
};

use serde::Serialize;
use tree_sitter::{Node, ParseOptions, ParseState, Parser, Point, Range, Tree};

mod contexts;
mod lower;

pub use contexts::{
    BashCommandContext, BashCommandContexts, BashContextAssumptions, BashContextDiagnostic,
    BashContextError, BashContextIssue, BashCwdSet, MAX_CONTEXT_BYTES, MAX_CWD_PATH_BYTES,
    MAX_CWD_STATES,
};

pub const BASH_ANALYSIS_VERSION: u16 = 2;
pub const BASH_GRAMMAR_VERSION: &str = "tree-sitter-bash-0.25.1";
pub const MAX_BASH_SOURCE_BYTES: usize = 64 * 1024;
pub const MAX_BASH_CST_NODES: usize = 4096;
pub const MAX_BASH_DEPTH: usize = 64;
pub const MAX_BASH_PARSE_MILLIS: u64 = 250;
pub const MAX_BASH_REGION_COMMANDS: usize = 256;
pub const MAX_BASH_REGION_ARGV_WORDS: usize = 128;
pub const MAX_BASH_REGION_ARGV_BYTES: usize = 8 * 1024;

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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BashPayloadKind {
    /// Bash expands nothing in the body when any part of the delimiter word is quoted.
    Heredoc {
        quoted: bool,
    },
    HereString,
}

/// Data a heredoc or here-string feeds to a descriptor of its command.
///
/// `span` is the redirection on the command line and `body` the payload source. A heredoc body
/// and its delimiter line follow the command line, so they lie outside the command's span.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashPayload {
    pub span: BashSpan,
    pub operator: BashSpan,
    pub descriptor: Option<BashSpan>,
    pub body: BashSpan,
    pub kind: BashPayloadKind,
    /// The exact bytes the descriptor reads, present only when no expansion can change them: a
    /// heredoc body after `<<-` tab stripping, or a here-string word after quote removal followed
    /// by the newline Bash appends.
    pub literal: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "index", rename_all = "snake_case")]
pub enum BashCommandPart {
    Word(usize),
    Assignment(usize),
    Redirect(usize),
    Payload(usize),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashCommand {
    pub complete: bool,
    pub words: Vec<BashWord>,
    pub assignments: Vec<BashAssignment>,
    pub redirects: Vec<BashRedirect>,
    pub payloads: Vec<BashPayload>,
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

/// A command found inside an `UnsupportedSyntax` region, which stays unlowered.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashRegionCommand {
    pub region: BashNodeId,
    pub span: BashSpan,
    /// The decoded command name, or `None` when it is not literal.
    pub executable: Option<String>,
    /// Every word including the executable, present only when all of them are literal.
    pub argv: Option<Vec<String>>,
}

/// Commands inside every `UnsupportedSyntax` region, nested regions included, in source order.
///
/// `complete` is false when a bound was reached or a region holds commands the grammar does not
/// expose, such as backticks in an unquoted heredoc. A consumer must then assume a region can
/// run anything.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BashRegionInventory {
    pub commands: Vec<BashRegionCommand>,
    pub complete: bool,
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
    regions: BashRegionInventory,
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
    pub const fn region_inventory(&self) -> &BashRegionInventory {
        &self.regions
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
            .saturating_add(lower::retained_inventory_bytes(&self.regions))
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

/// Parses only `source[start..end]`, with node offsets that still index `source`.
pub(crate) fn parse_tree_range(
    source: &str,
    start: usize,
    end: usize,
) -> Result<Tree, BashParseError> {
    let started = Instant::now();
    let range = Range {
        start_byte: start,
        end_byte: end,
        start_point: point_at(source, start),
        end_point: point_at(source, end),
    };
    parse_tree_within(source, Some(range), &BashLimits::default(), || {
        started.elapsed()
    })
}

fn point_at(source: &str, offset: usize) -> Point {
    let before = &source.as_bytes()[..offset];
    let line_start = before
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    Point::new(
        before.iter().filter(|byte| **byte == b'\n').count(),
        offset - line_start,
    )
}

fn parse_tree_with_clock(
    source: &str,
    limits: &BashLimits,
    elapsed: impl FnMut() -> Duration,
) -> Result<Tree, BashParseError> {
    parse_tree_within(source, None, limits, elapsed)
}

fn parse_tree_within(
    source: &str,
    range: Option<Range>,
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
    if let Some(range) = range {
        parser
            .set_included_ranges(&[range])
            .map_err(|_| BashParseError::ParserUnavailable)?;
    }
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
        BashDiagnosticKind, BashLimits, BashNodeId, BashNodeKind, BashOperatorKind, BashParseError,
        BashPayloadKind, BashProgram, BashQuoting, BashRedirectKind, MAX_BASH_REGION_ARGV_BYTES,
        MAX_BASH_REGION_ARGV_WORDS, MAX_BASH_REGION_COMMANDS, MAX_BASH_SOURCE_BYTES, parse_bash,
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
        "cat <<EOF\n`touch never`\nEOF\n",
        "cat <<A <<B\none\nA\ntwo\nB\n",
        "cat - <<EOF\nx\nEOF\n",
        "cat -<<EOF\nx\nEOF\n",
        "cat <<-EOF\n\tx\n\tEOF\n",
        "cat <<E\"O\"F\n$x\nEOF\n",
        "cat <<EOF\nEOF\n",
        "FOO=1 python3 - <<EOF >log 2>&1\nx\nEOF\n",
        "python3 <<PY | cat tail && echo done\nx\nPY\n",
        "while read -r line; do echo \"$line\"; done <<EOF\nx\nEOF\n",
        "cat <<<payload",
        "cat <<< \"a b\" extra >out",
        "a | b | c && cd /tmp; rm x",
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

    /// The grouping Bash gives a lowered program, written with explicit parentheses.
    fn grouping(program: &BashProgram, id: BashNodeId) -> String {
        let join = |ids: &[BashNodeId], separator: &str| {
            ids.iter()
                .map(|id| grouping(program, *id))
                .collect::<Vec<_>>()
                .join(separator)
        };
        match &program.nodes()[id.0].structure {
            BashNodeKind::Sequence { items, .. } => join(items, "; "),
            BashNodeKind::AndOr {
                left,
                operator,
                right,
            } => format!(
                "({} {} {})",
                grouping(program, *left),
                program.text(&operator.span).unwrap(),
                grouping(program, *right)
            ),
            BashNodeKind::Pipeline { commands, .. } => format!("({})", join(commands, " | ")),
            BashNodeKind::Command { command } => command.static_argv().unwrap().join(" "),
            other => panic!("unexpected structure {other:?}"),
        }
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
    fn heredoc_commands_lower_completely() {
        const QUOTED: BashPayloadKind = BashPayloadKind::Heredoc { quoted: true };
        const UNQUOTED: BashPayloadKind = BashPayloadKind::Heredoc { quoted: false };
        for (source, argv, kind, body, literal) in [
            (
                HEREDOC,
                &["python3", "-"][..],
                QUOTED,
                "print('one')\n",
                Some("print('one')\n"),
            ),
            (
                "cat - <<EOF\nx\nEOF\n",
                &["cat", "-"],
                UNQUOTED,
                "x\n",
                Some("x\n"),
            ),
            (
                "cat -<<EOF\nx\nEOF\n",
                &["cat", "-"],
                UNQUOTED,
                "x\n",
                Some("x\n"),
            ),
            (
                "cat <<-EOF\n\tone\n\t\ttwo\n\tEOF\n",
                &["cat"],
                UNQUOTED,
                "\tone\n\t\ttwo\n",
                Some("one\ntwo\n"),
            ),
            (
                "cat <<EOF\n$HOME\nEOF\n",
                &["cat"],
                UNQUOTED,
                "$HOME\n",
                None,
            ),
            (
                "cat <<EOF\na\\\nb\nEOF\n",
                &["cat"],
                UNQUOTED,
                "a\\\nb\n",
                None,
            ),
            (
                "cat <<'EOF'\n$HOME `id`\nEOF\n",
                &["cat"],
                QUOTED,
                "$HOME `id`\n",
                Some("$HOME `id`\n"),
            ),
            (
                "cat <<E\"O\"F\n$HOME\nEOF\n",
                &["cat"],
                QUOTED,
                "$HOME\n",
                Some("$HOME\n"),
            ),
            ("cat <<EOF\nEOF\n", &["cat"], UNQUOTED, "", Some("")),
            (
                "FOO=1 python3 - <<EOF >log 2>&1\nx\nEOF\n",
                &["python3", "-"],
                UNQUOTED,
                "x\n",
                Some("x\n"),
            ),
            (
                "cat 3<<EOF\nx\nEOF\n",
                &["cat"],
                UNQUOTED,
                "x\n",
                Some("x\n"),
            ),
            (
                "cat <<< \"a b\" extra",
                &["cat", "extra"],
                BashPayloadKind::HereString,
                "\"a b\"",
                Some("a b\n"),
            ),
            (
                "cat <<< $HOME",
                &["cat"],
                BashPayloadKind::HereString,
                "$HOME",
                None,
            ),
        ] {
            let program = parse_bash(source).unwrap();
            assert!(
                program.is_complete(),
                "{source:?}: {:?}",
                program.diagnostics()
            );
            let command = first_command(&program);
            assert_eq!(command.static_argv().unwrap(), argv, "{source:?}");
            let [payload] = &command.payloads[..] else {
                panic!("{source:?} has one payload");
            };
            assert_eq!(payload.kind, kind, "{source:?}");
            assert_eq!(program.text(&payload.body), Some(body), "{source:?}");
            assert_eq!(payload.literal.as_deref(), literal, "{source:?}");
            assert!(
                program
                    .coverage()
                    .iter()
                    .any(|covered| covered.span == payload.body
                        && covered.role == BashCoverageKind::Payload)
                    || body.is_empty(),
                "{source:?}"
            );
            assert_coverage(&program);
        }
        let assigned = parse_bash("FOO=1 python3 - <<EOF >log 2>&1\nx\nEOF\n").unwrap();
        let command = first_command(&assigned);
        assert_eq!(command.assignments.len(), 1);
        assert_eq!(command.redirects.len(), 2);
        let descriptor = parse_bash("cat 3<<EOF\nx\nEOF\n").unwrap();
        let payload = &first_command(&descriptor).payloads[0];
        assert_eq!(
            descriptor.text(payload.descriptor.as_ref().unwrap()),
            Some("3")
        );
        assert_eq!(descriptor.text(&payload.span), Some("3<<EOF"));
        let other = parse_bash(&HEREDOC.replace("one", "two")).unwrap();
        assert_ne!(parse_bash(HEREDOC).unwrap(), other);
    }

    #[test]
    fn heredoc_tails_stay_visible() {
        for (source, expected) in [
            (
                "python3 <<'PY' && cat tail\nprint('one')\nPY\n",
                "(python3 && cat tail)",
            ),
            (
                "python3 <<'PY' || cat tail\nprint('one')\nPY\n",
                "(python3 || cat tail)",
            ),
            (
                "python3 <<'PY' | cat tail\nprint('one')\nPY\n",
                "(python3 | cat tail)",
            ),
            ("python3 <<'PY' >out arg\nprint('one')\nPY\n", "python3 arg"),
            ("cat <<EOF >out && rm x\nx\nEOF\n", "(cat && rm x)"),
            (
                "cat <<'EOF' | sudo tee /etc/x\nx\nEOF\n",
                "(cat | sudo tee /etc/x)",
            ),
            (
                "python3 <<PY | cat tail && echo done\nx\nPY\n",
                "((python3 | cat tail) && echo done)",
            ),
            ("python3 <<PY && a || b\nx\nPY\n", "((python3 && a) || b)"),
            ("a && python3 - <<PY | b\nx\nPY\n", "(a && (python3 - | b))"),
            ("a | b && c - <<PY\nx\nPY\n", "((a | b) && c -)"),
        ] {
            let program = parse_bash(source).unwrap();
            assert!(
                program.is_complete(),
                "{source:?}: {:?}",
                program.diagnostics()
            );
            assert_eq!(grouping(&program, program.root()), expected, "{source:?}");
            let fed: Vec<_> = program
                .commands()
                .filter(|(_, command)| !command.payloads.is_empty())
                .collect();
            let [(id, command)] = fed[..] else {
                panic!("{source:?} feeds one command");
            };
            assert!(
                program.nodes()[id.0].span.end < command.payloads[0].body.start,
                "{source:?}"
            );
            assert_coverage(&program);
        }
    }

    #[test]
    fn pipelines_bind_tighter_than_and_or_lists() {
        for (source, expected) in [
            (
                "a | b | c && cd /tmp; rm x",
                "((a | b | c) && cd /tmp); rm x",
            ),
            ("a | b || c && d", "(((a | b) || c) && d)"),
            ("a && b | c && d", "((a && (b | c)) && d)"),
            ("a | b && c | d", "((a | b) && (c | d))"),
            ("a || b && c | d; e", "((a || b) && (c | d)); e"),
        ] {
            let program = parse_bash(source).unwrap();
            assert!(
                program.is_complete(),
                "{source:?}: {:?}",
                program.diagnostics()
            );
            assert_eq!(grouping(&program, program.root()), expected, "{source:?}");
            assert_coverage(&program);
        }
    }

    #[test]
    fn redirects_after_a_list_belong_to_its_last_command() {
        for (source, expected, redirected) in [
            ("a && b >x c", "(a && b c)", "b c"),
            (
                "cd x && cargo test 2>&1",
                "(cd x && cargo test)",
                "cargo test",
            ),
            ("a | b 2>/dev/null", "(a | b)", "b"),
            ("a || b | c >out && d", "((a || (b | c)) && d)", "c"),
            ("a && b >x || c", "((a && b) || c)", "b"),
        ] {
            let program = parse_bash(source).unwrap();
            assert!(
                program.is_complete(),
                "{source:?}: {:?}",
                program.diagnostics()
            );
            assert_eq!(grouping(&program, program.root()), expected, "{source:?}");
            let with_redirects: Vec<_> = program
                .commands()
                .filter(|(_, command)| !command.redirects.is_empty())
                .map(|(_, command)| command.static_argv().unwrap().join(" "))
                .collect();
            assert_eq!(with_redirects, [redirected], "{source:?}");
            assert_coverage(&program);
        }
    }

    #[test]
    fn unsupported_regions_list_their_commands() {
        let listed = |program: &BashProgram| {
            program
                .region_inventory()
                .commands
                .iter()
                .map(|command| {
                    (
                        command.executable.clone(),
                        command.argv.as_ref().map(|argv| argv.join(" ")),
                    )
                })
                .collect::<Vec<_>>()
        };
        let named = |executable: &str, argv: Option<&str>| {
            (Some(executable.to_owned()), argv.map(str::to_owned))
        };
        for (source, expected) in [
            (
                "for x in a b; do sudo cat $x; done",
                vec![named("sudo", None)],
            ),
            (
                "echo $(sudo id) `whoami`",
                vec![
                    named("echo", None),
                    named("sudo", Some("sudo id")),
                    named("whoami", Some("whoami")),
                ],
            ),
            ("f() { \"$runner\" x; }", vec![(None, None)]),
            (
                "if true; then env >/dev/null sudo id; fi",
                vec![
                    named("true", Some("true")),
                    named("env", Some("env sudo id")),
                ],
            ),
            (
                "if true; then a && env >/dev/null sudo id; fi",
                vec![
                    named("true", Some("true")),
                    named("a", Some("a")),
                    named("env", Some("env sudo id")),
                ],
            ),
            (
                "if true; then python3 - <<PY\nprint()\nPY\nfi",
                vec![named("true", Some("true")), named("python3", None)],
            ),
            (
                "while read -r line; do echo \"$line\"; done <<EOF\nx\nEOF\n",
                vec![named("read", Some("read -r line")), named("echo", None)],
            ),
            (
                "cat <<EOF\n$(sudo id)\nEOF\n",
                vec![named("cat", Some("cat")), named("sudo", Some("sudo id"))],
            ),
        ] {
            let program = parse_bash(source).unwrap();
            assert!(!program.is_complete(), "{source:?}");
            assert!(program.region_inventory().complete, "{source:?}");
            assert_eq!(listed(&program), expected, "{source:?}");
            assert!(
                program
                    .region_inventory()
                    .commands
                    .iter()
                    .all(|command| matches!(
                        program.nodes()[command.region.0].structure,
                        BashNodeKind::Unknown { .. }
                    )),
                "{source:?}"
            );
        }
        for source in [
            "cat <<EOF\n`sudo id`\nEOF\n".to_owned(),
            "if true; then cat <<EOF\n`sudo id`\nEOF\nfi".to_owned(),
            format!(
                "for x in a; do {}done",
                "c; ".repeat(MAX_BASH_REGION_COMMANDS + 1)
            ),
            format!(
                "for x in a; do echo{}; done",
                " w".repeat(MAX_BASH_REGION_ARGV_WORDS)
            ),
            format!(
                "for x in a; do echo {}; done",
                "w".repeat(MAX_BASH_REGION_ARGV_BYTES)
            ),
        ] {
            let program = parse_bash(&source).unwrap();
            assert!(!program.region_inventory().complete, "{source:?}");
            assert!(
                program.region_inventory().commands.len() <= MAX_BASH_REGION_COMMANDS,
                "{source:?}"
            );
        }
        let lowered = parse_bash("cat a && cat b").unwrap();
        assert!(lowered.region_inventory().commands.is_empty());
        assert!(lowered.region_inventory().complete);
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
            "cat <<EOF\n$(cat secret)\nEOF\n",
            "cat <<EOF\n`cat secret`\nEOF\n",
            "cat <<< \"$(cat secret)\"",
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
                    timeout_sec: None,
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
                assert_eq!(
                    prepared.analysis().scopes[0].arguments,
                    Some(vec![ShellWord::Literal("-".into())])
                );
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
