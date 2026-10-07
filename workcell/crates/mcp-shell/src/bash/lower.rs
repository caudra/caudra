use std::mem::{replace, take};

use tree_sitter::Node;

use super::{
    BASH_ANALYSIS_VERSION, BASH_GRAMMAR_VERSION, BashAssignment, BashCommand, BashCommandPart,
    BashCoverage, BashCoverageKind, BashDiagnostic, BashDiagnosticKind, BashFragmentValue,
    BashNode, BashNodeId, BashNodeKind, BashOperator, BashOperatorKind, BashPayload,
    BashPayloadKind, BashProgram, BashQuoting, BashRedirect, BashRedirectKind, BashRegionCommand,
    BashRegionInventory, BashSpan, BashWord, BashWordFragment, MAX_BASH_REGION_ARGV_BYTES,
    MAX_BASH_REGION_ARGV_WORDS, MAX_BASH_REGION_COMMANDS, parse_tree_range,
};

const HEREDOC_OPERATORS: [&str; 2] = ["<<", "<<-"];
const TAB_STRIPPING_OPERATOR: &str = "<<-";
const HERESTRING_OPERATOR: &str = "<<<";
const DELIMITER_QUOTES: [char; 3] = ['\'', '"', '\\'];
const BODY_EXPANSIONS: [char; 3] = ['$', '`', '\\'];
const BACKTICK: char = '`';

pub(super) fn lower(source: &str, root: Node<'_>) -> BashProgram {
    let mut program = BashProgram {
        analysis_version: BASH_ANALYSIS_VERSION,
        grammar_version: BASH_GRAMMAR_VERSION,
        source: source.to_owned(),
        root: BashNodeId(0),
        nodes: Vec::new(),
        coverage: Vec::new(),
        diagnostics: Vec::new(),
        regions: BashRegionInventory {
            commands: Vec::new(),
            complete: true,
        },
    };
    program.root = program.lower_node(root);
    program.finish_coverage();
    program
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

fn trivia(source: &str) -> bool {
    let mut bytes = source.as_bytes();
    while !bytes.is_empty() {
        bytes = match bytes {
            [b' ' | b'\t' | b'\n', rest @ ..] | [b'\\', b'\n', rest @ ..] => rest,
            _ => return false,
        };
    }
    true
}

fn unsupported(node: Node<'_>) -> BashDiagnosticKind {
    BashDiagnosticKind::UnsupportedSyntax(node.kind().to_owned())
}

fn new_command() -> BashCommand {
    BashCommand {
        complete: true,
        words: Vec::new(),
        assignments: Vec::new(),
        redirects: Vec::new(),
        payloads: Vec::new(),
        parts: Vec::new(),
    }
}

fn part_span<'a>(command: &'a BashCommand, part: &BashCommandPart) -> &'a BashSpan {
    match part {
        BashCommandPart::Word(index) => &command.words[*index].span,
        BashCommandPart::Assignment(index) => &command.assignments[*index].span,
        BashCommandPart::Redirect(index) => &command.redirects[*index].span,
        BashCommandPart::Payload(index) => &command.payloads[*index].span,
    }
}

fn finished(node: Node<'_>, mut command: BashCommand) -> Result<BashCommand, BashDiagnosticKind> {
    if command.words.is_empty() {
        return Err(unsupported(node));
    }
    let mut parts = take(&mut command.parts);
    parts.sort_by_key(|part| part_span(&command, part).start);
    command.parts = parts;
    Ok(command)
}

/// The line a command occupies. A heredoc body follows it, outside this span.
fn command_line(command: &BashCommand) -> BashSpan {
    let spans = || command.parts.iter().map(|part| part_span(command, part));
    BashSpan {
        start: spans().map(|span| span.start).min().unwrap_or_default(),
        end: spans().map(|span| span.end).max().unwrap_or_default(),
    }
}

fn heredoc_redirect(statement: Node<'_>) -> Option<Node<'_>> {
    children(statement)
        .into_iter()
        .find(|child| child.kind() == "heredoc_redirect")
}

/// Whether the rest of a heredoc's list or pipeline sits inside the heredoc's own node.
fn heredoc_tail(redirect: Node<'_>) -> bool {
    children(redirect).iter().enumerate().any(|(index, child)| {
        child.kind() == "pipeline"
            || matches!(
                redirect.field_name_for_child(index as u32),
                Some("operator" | "right")
            )
    })
}

/// Whether a redirected statement continues a list or pipeline. The grammar attaches the
/// redirects written after `a && b` to the whole list, though they belong to `b`, and a heredoc's
/// node holds the rest of its line.
fn chained(statement: Node<'_>) -> bool {
    statement
        .child_by_field_name("body")
        .is_some_and(|body| matches!(body.kind(), "list" | "pipeline"))
        || heredoc_redirect(statement).is_some_and(heredoc_tail)
}

/// Whether redirects written after `parent` belong to `node`, the command that ends it.
fn ends(parent: Node<'_>, node: Node<'_>) -> bool {
    match parent.kind() {
        "redirected_statement" => parent.child_by_field_name("body") == Some(node),
        "list" | "pipeline" => children(parent).last() == Some(&node),
        _ => false,
    }
}

fn field_children<'tree>(node: Node<'tree>, field: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field, &mut cursor).collect()
}

fn strip_leading_tabs(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| line.trim_start_matches('\t'))
        .collect()
}

impl BashProgram {
    fn claim(&mut self, span: BashSpan, owner: BashNodeId, role: BashCoverageKind) {
        if span.start < span.end {
            self.coverage.push(BashCoverage { span, owner, role });
        }
    }

    fn add_node(&mut self, span: BashSpan, structure: BashNodeKind) -> BashNodeId {
        let id = BashNodeId(self.nodes.len());
        self.nodes.push(BashNode { span, structure });
        id
    }

    fn span_between(&self, first: BashNodeId, last: BashNodeId) -> BashSpan {
        BashSpan {
            start: self.nodes[first.0].span.start,
            end: self.nodes[last.0].span.end,
        }
    }

    fn gaps_are_trivia(&self, node: Node<'_>) -> bool {
        let mut offset = node.start_byte();
        for child in children(node) {
            if child.start_byte() < offset || !trivia(&self.source[offset..child.start_byte()]) {
                return false;
            }
            offset = child.end_byte();
        }
        trivia(&self.source[offset..node.end_byte()])
    }

    fn lower_node(&mut self, node: Node<'_>) -> BashNodeId {
        let id = self.add_node(
            BashSpan::of(node),
            BashNodeKind::Unknown {
                reason: unsupported(node),
            },
        );
        let diagnostics_start = self.diagnostics.len();
        let coverage_start = self.coverage.len();
        let regions_start = self.regions.commands.len();
        let regions_complete = self.regions.complete;
        let result = if node.has_error() || node.is_missing() {
            Err(BashDiagnosticKind::SyntaxError)
        } else {
            self.lower_structure(node, id)
        };
        match result {
            Ok(structure) => self.nodes[id.0].structure = structure,
            Err(reason) => {
                let span = BashSpan::of(node);
                self.nodes.truncate(id.0 + 1);
                self.diagnostics.truncate(diagnostics_start);
                self.coverage.truncate(coverage_start);
                self.regions.commands.truncate(regions_start);
                self.regions.complete = regions_complete;
                if matches!(reason, BashDiagnosticKind::UnsupportedSyntax(_)) {
                    self.record_region(node, id);
                }
                self.diagnostics.push(BashDiagnostic {
                    span: span.clone(),
                    kind: reason.clone(),
                });
                self.nodes[id.0].structure = BashNodeKind::Unknown { reason };
                self.claim_unknown_with_payloads(node, id);
            }
        }
        id
    }

    fn lower_structure(
        &mut self,
        node: Node<'_>,
        id: BashNodeId,
    ) -> Result<BashNodeKind, BashDiagnosticKind> {
        match node.kind() {
            "program" => self.sequence(node, id, false),
            "redirected_statement" if chained(node) => self.chain(node, id),
            "redirected_statement" if heredoc_redirect(node).is_some() => {
                let (command, _) = self.statement_command(node, node.start_byte(), id)?;
                self.nodes[id.0].span = command_line(&command);
                Ok(BashNodeKind::Command { command })
            }
            "command" | "redirected_statement" => {
                let mut command = new_command();
                self.command_parts(node, id, &mut command)?;
                Ok(BashNodeKind::Command {
                    command: finished(node, command)?,
                })
            }
            "list" | "pipeline" => self.chain(node, id),
            "subshell" | "compound_statement" => {
                let direct = children(node);
                let (Some(first), Some(last)) = (direct.first(), direct.last()) else {
                    return Err(unsupported(node));
                };
                if !matches!((first.kind(), last.kind()), ("(", ")") | ("{", "}")) {
                    return Err(unsupported(node));
                }
                let Some(open) = self.operator(*first, id) else {
                    return Err(unsupported(node));
                };
                let Some(close) = self.operator(*last, id) else {
                    return Err(unsupported(node));
                };
                let structure = self.sequence(node, id, true)?;
                let body = self.add_node(
                    BashSpan {
                        start: first.end_byte(),
                        end: last.start_byte(),
                    },
                    structure,
                );
                if node.kind() == "subshell" {
                    Ok(BashNodeKind::Subshell { body, open, close })
                } else {
                    Ok(BashNodeKind::BraceGroup { body, open, close })
                }
            }
            "variable_assignment" => Ok(BashNodeKind::Assignments {
                assignments: vec![self.assignment(node, id)?],
            }),
            "variable_assignments" => {
                if !self.gaps_are_trivia(node) {
                    return Err(BashDiagnosticKind::SourceGap);
                }
                let assignments = children(node)
                    .into_iter()
                    .map(|child| self.assignment(child, id))
                    .collect::<Result<_, _>>()?;
                Ok(BashNodeKind::Assignments { assignments })
            }
            _ => Err(unsupported(node)),
        }
    }

    fn sequence(
        &mut self,
        node: Node<'_>,
        id: BashNodeId,
        delimited: bool,
    ) -> Result<BashNodeKind, BashDiagnosticKind> {
        if !self.gaps_are_trivia(node) {
            return Err(BashDiagnosticKind::SourceGap);
        }
        let mut items: Vec<BashNodeId> = Vec::new();
        let mut separators = Vec::new();
        let mut offset = node.start_byte();
        let direct = children(node);
        for (index, child) in direct.iter().enumerate() {
            self.newlines(offset, child.start_byte(), id, &mut separators);
            offset = child.end_byte();
            if delimited && (index == 0 || index + 1 == direct.len()) {
                continue;
            }
            if child.kind() == "comment" {
                self.claim(BashSpan::of(*child), id, BashCoverageKind::Trivia);
                continue;
            }
            if let Some(operator) = self.operator(*child, id) {
                if operator.kind == BashOperatorKind::Background {
                    let Some(body) = items.pop() else {
                        return Err(unsupported(node));
                    };
                    let span = BashSpan {
                        start: self.nodes[body.0].span.start,
                        end: operator.span.end,
                    };
                    items.push(self.add_node(
                        span,
                        BashNodeKind::Background {
                            body,
                            operator: operator.clone(),
                        },
                    ));
                } else if !matches!(
                    operator.kind,
                    BashOperatorKind::Semicolon | BashOperatorKind::Newline
                ) {
                    return Err(unsupported(*child));
                }
                separators.push(operator);
            } else if child.is_named() {
                items.push(self.lower_node(*child));
            } else {
                return Err(unsupported(*child));
            }
        }
        self.newlines(offset, node.end_byte(), id, &mut separators);
        Ok(BashNodeKind::Sequence { items, separators })
    }

    /// Lowers an and-or list with Bash's precedence, in which pipelines bind tighter. The grammar
    /// can nest a list inside a pipeline, as in `a | b | c && cd x`, and a heredoc's node holds the
    /// rest of its line, so elements and operators are read in source order and grouped again.
    fn chain(
        &mut self,
        node: Node<'_>,
        id: BashNodeId,
    ) -> Result<BashNodeKind, BashDiagnosticKind> {
        let mut elements = Vec::new();
        let mut operators = Vec::new();
        self.links(node, id, &mut elements, &mut operators)?;
        if elements.len() != operators.len() + 1 {
            return Err(unsupported(node));
        }
        let mut elements = elements.into_iter();
        let Some(first) = elements.next() else {
            return Err(unsupported(node));
        };
        let mut pipeline = (vec![first], Vec::new());
        let mut pipelines = Vec::new();
        let mut joins = Vec::new();
        for (operator, element) in operators.into_iter().zip(elements) {
            if matches!(
                operator.kind,
                BashOperatorKind::Pipe | BashOperatorKind::PipeStderr
            ) {
                pipeline.0.push(element);
                pipeline.1.push(operator);
            } else {
                pipelines.push(replace(&mut pipeline, (vec![element], Vec::new())));
                joins.push(operator);
            }
        }
        if joins.is_empty() {
            let (commands, operators) = pipeline;
            return Ok(BashNodeKind::Pipeline {
                commands,
                operators,
            });
        }
        pipelines.push(pipeline);
        let mut pipelines = pipelines
            .into_iter()
            .map(|(commands, operators)| self.pipeline(commands, operators))
            .collect::<Vec<_>>()
            .into_iter();
        let mut joins = joins.into_iter();
        let (Some(mut left), Some(operator), Some(right)) =
            (pipelines.next(), joins.next_back(), pipelines.next_back())
        else {
            return Err(unsupported(node));
        };
        for (join, next) in joins.zip(pipelines) {
            let span = self.span_between(left, next);
            left = self.add_node(
                span,
                BashNodeKind::AndOr {
                    left,
                    operator: join,
                    right: next,
                },
            );
        }
        Ok(BashNodeKind::AndOr {
            left,
            operator,
            right,
        })
    }

    fn pipeline(&mut self, commands: Vec<BashNodeId>, operators: Vec<BashOperator>) -> BashNodeId {
        if let [single] = commands[..] {
            return single;
        }
        let span = self.span_between(commands[0], commands[commands.len() - 1]);
        self.add_node(
            span,
            BashNodeKind::Pipeline {
                commands,
                operators,
            },
        )
    }

    fn links(
        &mut self,
        node: Node<'_>,
        owner: BashNodeId,
        elements: &mut Vec<BashNodeId>,
        operators: &mut Vec<BashOperator>,
    ) -> Result<(), BashDiagnosticKind> {
        if matches!(node.kind(), "list" | "pipeline") {
            if !self.gaps_are_trivia(node) {
                return Err(BashDiagnosticKind::SourceGap);
            }
            return self.link_parts(children(node), owner, elements, operators);
        }
        if node.kind() == "redirected_statement" && chained(node) {
            let Some(body) = node.child_by_field_name("body") else {
                return Err(unsupported(node));
            };
            let start = self.link_body(body, owner, elements, operators)?;
            let id = self.add_node(
                BashSpan::of(node),
                BashNodeKind::Unknown {
                    reason: unsupported(node),
                },
            );
            let (command, tail) = self.statement_command(node, start, id)?;
            self.nodes[id.0] = BashNode {
                span: command_line(&command),
                structure: BashNodeKind::Command { command },
            };
            elements.push(id);
            return self.link_parts(tail, owner, elements, operators);
        }
        elements.push(self.lower_node(node));
        Ok(())
    }

    /// Links a redirected statement's list or pipeline up to the command that ends it, and
    /// returns where that command starts.
    fn link_body(
        &mut self,
        body: Node<'_>,
        owner: BashNodeId,
        elements: &mut Vec<BashNodeId>,
        operators: &mut Vec<BashOperator>,
    ) -> Result<usize, BashDiagnosticKind> {
        if !matches!(body.kind(), "list" | "pipeline") {
            return Ok(body.start_byte());
        }
        if !self.gaps_are_trivia(body) {
            return Err(BashDiagnosticKind::SourceGap);
        }
        let mut parts = children(body);
        let Some(last) = parts.pop() else {
            return Err(unsupported(body));
        };
        self.link_parts(parts, owner, elements, operators)?;
        self.link_body(last, owner, elements, operators)
    }

    fn link_parts(
        &mut self,
        parts: Vec<Node<'_>>,
        owner: BashNodeId,
        elements: &mut Vec<BashNodeId>,
        operators: &mut Vec<BashOperator>,
    ) -> Result<(), BashDiagnosticKind> {
        for part in parts {
            if part.kind() == "comment" {
                self.claim(BashSpan::of(part), owner, BashCoverageKind::Trivia);
            } else if let Some(operator) = self.operator(part, owner) {
                if !matches!(
                    operator.kind,
                    BashOperatorKind::And
                        | BashOperatorKind::Or
                        | BashOperatorKind::Pipe
                        | BashOperatorKind::PipeStderr
                ) {
                    return Err(unsupported(part));
                }
                operators.push(operator);
            } else if part.is_named() {
                self.links(part, owner, elements, operators)?;
            } else {
                return Err(unsupported(part));
            }
        }
        Ok(())
    }

    /// The command starting at `start` that a redirected statement's redirects belong to, and the
    /// nodes after a heredoc on its line that continue the command's list or pipeline.
    fn statement_command<'tree>(
        &mut self,
        statement: Node<'tree>,
        start: usize,
        id: BashNodeId,
    ) -> Result<(BashCommand, Vec<Node<'tree>>), BashDiagnosticKind> {
        if let Some(redirect) = heredoc_redirect(statement) {
            return self.heredoc_command(statement, redirect, start, id);
        }
        let mut command = new_command();
        self.range_command(start, statement.end_byte(), id, &mut command)?;
        Ok((finished(statement, command)?, Vec::new()))
    }

    fn heredoc_command<'tree>(
        &mut self,
        statement: Node<'tree>,
        redirect: Node<'tree>,
        start: usize,
        id: BashNodeId,
    ) -> Result<(BashCommand, Vec<Node<'tree>>), BashDiagnosticKind> {
        if children(statement).last() != Some(&redirect) {
            return Err(unsupported(statement));
        }
        if !self.gaps_are_trivia(redirect) {
            return Err(BashDiagnosticKind::SourceGap);
        }
        let direct = children(redirect);
        let find = |kind: &str| direct.iter().copied().find(|child| child.kind() == kind);
        let (Some(operator), Some(delimiter), Some(body), Some(end)) = (
            direct
                .iter()
                .copied()
                .find(|child| HEREDOC_OPERATORS.contains(&child.kind())),
            find("heredoc_start"),
            find("heredoc_body"),
            find("heredoc_end"),
        ) else {
            return Err(unsupported(redirect));
        };
        let operator_span = BashSpan {
            start: operator.end_byte().saturating_sub(operator.kind().len()),
            end: operator.end_byte(),
        };
        if self.text(&operator_span) != Some(operator.kind()) {
            return Err(BashDiagnosticKind::SourceGap);
        }
        let descriptor = redirect.child_by_field_name("descriptor").map(BashSpan::of);
        let line_start = descriptor
            .as_ref()
            .map_or(operator_span.start, |span| span.start);
        let mut command = new_command();
        self.range_command(start, line_start, id, &mut command)?;
        let mut tail = Vec::new();
        for (index, child) in direct.iter().copied().enumerate() {
            match (child.kind(), redirect.field_name_for_child(index as u32)) {
                ("pipeline", _) | (_, Some("operator" | "right")) => tail.push(child),
                ("file_redirect", _) => {
                    reject_substitutions(child)?;
                    self.redirect(child, id, &mut command)?;
                }
                ("herestring_redirect", _) => {
                    reject_substitutions(child)?;
                    self.herestring(child, id, &mut command)?;
                }
                (_, Some("argument")) => {
                    reject_substitutions(child)?;
                    self.command_word(child, id, &mut command);
                }
                ("comment", _) => self.claim(BashSpan::of(child), id, BashCoverageKind::Trivia),
                ("heredoc_start" | "heredoc_body" | "heredoc_end", _) | (_, Some("descriptor")) => {
                }
                _ if child == operator => {}
                _ => return Err(unsupported(child)),
            }
        }
        reject_substitutions(body)?;
        if self.hides_commands(redirect) {
            return Err(unsupported(body));
        }
        let strip_tabs = operator.kind() == TAB_STRIPPING_OPERATOR;
        let (Some(body_start), Some(body_end)) = (
            self.line_start(body.start_byte(), strip_tabs),
            self.line_start(end.start_byte(), strip_tabs),
        ) else {
            return Err(BashDiagnosticKind::SourceGap);
        };
        if body_end < body_start {
            return Err(BashDiagnosticKind::SourceGap);
        }
        let quoted = self.delimiter_quoted(redirect);
        let text = &self.source[body_start..body_end];
        let literal = (quoted || !text.contains(BODY_EXPANSIONS)).then(|| {
            if strip_tabs {
                strip_leading_tabs(text)
            } else {
                text.to_owned()
            }
        });
        let span = BashSpan {
            start: line_start,
            end: delimiter.end_byte(),
        };
        let body = BashSpan {
            start: body_start,
            end: body_end,
        };
        self.claim(span.clone(), id, BashCoverageKind::Redirect);
        self.claim(body.clone(), id, BashCoverageKind::Payload);
        self.claim(BashSpan::of(end), id, BashCoverageKind::Redirect);
        command
            .parts
            .push(BashCommandPart::Payload(command.payloads.len()));
        command.payloads.push(BashPayload {
            span,
            operator: operator_span,
            descriptor,
            body,
            kind: BashPayloadKind::Heredoc { quoted },
            literal,
        });
        Ok((finished(statement, command)?, tail))
    }

    /// Lowers the source between `start` and `end` as one command from a parse of its own.
    /// tree-sitter-bash leaves a bare `-` written just before a heredoc, as in `python3 - <<EOF`,
    /// out of the statement's tree, and attaches a list's redirects to the whole list.
    fn range_command(
        &mut self,
        start: usize,
        end: usize,
        id: BashNodeId,
        command: &mut BashCommand,
    ) -> Result<(), BashDiagnosticKind> {
        let tree = parse_tree_range(&self.source, start, end)
            .map_err(|_| BashDiagnosticKind::SourceGap)?;
        let root = tree.root_node();
        if root.has_error() {
            return Err(BashDiagnosticKind::SyntaxError);
        }
        let [statement] = children(root)[..] else {
            return Err(BashDiagnosticKind::SourceGap);
        };
        if ![start..statement.start_byte(), statement.end_byte()..end]
            .into_iter()
            .all(|range| self.source.get(range).is_some_and(trivia))
        {
            return Err(BashDiagnosticKind::SourceGap);
        }
        if !matches!(statement.kind(), "command" | "redirected_statement") {
            return Err(unsupported(statement));
        }
        self.command_parts(statement, id, command)
    }

    /// Where the line holding `offset` starts, allowing only the tabs `<<-` strips before it.
    fn line_start(&self, offset: usize, strip_tabs: bool) -> Option<usize> {
        let before = self.source.get(..offset)?;
        let start = if strip_tabs {
            before.trim_end_matches('\t').len()
        } else {
            offset
        };
        before[..start].ends_with('\n').then_some(start)
    }

    fn delimiter_quoted(&self, heredoc: Node<'_>) -> bool {
        children(heredoc).iter().any(|child| {
            child.kind() == "heredoc_start"
                && self.source[child.byte_range()].contains(DELIMITER_QUOTES)
        })
    }

    /// Bash runs backticks in an unquoted heredoc body, but the grammar leaves them in its text.
    fn hides_commands(&self, heredoc: Node<'_>) -> bool {
        !self.delimiter_quoted(heredoc)
            && children(heredoc).iter().any(|child| {
                child.kind() == "heredoc_body" && self.source[child.byte_range()].contains(BACKTICK)
            })
    }

    fn herestring(
        &mut self,
        node: Node<'_>,
        id: BashNodeId,
        command: &mut BashCommand,
    ) -> Result<(), BashDiagnosticKind> {
        if !self.gaps_are_trivia(node) {
            return Err(BashDiagnosticKind::SourceGap);
        }
        let descriptor = node.child_by_field_name("descriptor").map(BashSpan::of);
        let direct = children(node);
        let (Some(operator), Some(body)) = (direct.iter().rev().nth(1), direct.last()) else {
            return Err(unsupported(node));
        };
        if operator.kind() != HERESTRING_OPERATOR
            || direct.len() != usize::from(descriptor.is_some()) + 2
        {
            return Err(unsupported(node));
        }
        let span = BashSpan::of(node);
        let operator = BashSpan::of(*operator);
        let literal = decode_static_word(&self.source[body.byte_range()]).map(|mut value| {
            value.push('\n');
            value
        });
        self.claim(
            BashSpan {
                start: span.start,
                end: operator.end,
            },
            id,
            BashCoverageKind::Redirect,
        );
        self.claim(BashSpan::of(*body), id, BashCoverageKind::Payload);
        command
            .parts
            .push(BashCommandPart::Payload(command.payloads.len()));
        command.payloads.push(BashPayload {
            span,
            operator,
            descriptor,
            body: BashSpan::of(*body),
            kind: BashPayloadKind::HereString,
            literal,
        });
        Ok(())
    }

    /// Lists the commands inside a region left unlowered, so a consumer can still find a command
    /// that always needs review there.
    fn record_region(&mut self, node: Node<'_>, region: BashNodeId) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "command" => self.record_region_command(current, region),
                "heredoc_redirect" if self.hides_commands(current) => {
                    self.regions.complete = false;
                }
                _ => {}
            }
            stack.extend(children(current).into_iter().rev());
        }
    }

    fn record_region_command(&mut self, node: Node<'_>, region: BashNodeId) {
        if self.regions.commands.len() == MAX_BASH_REGION_COMMANDS {
            self.regions.complete = false;
            return;
        }
        let (words, omitted) = self.region_words(node);
        let hides_words = !omitted.is_empty();
        let hides_commands = omitted
            .iter()
            .any(|text| decode_static_word(text.trim()).is_none());
        let bounded = words.len() <= MAX_BASH_REGION_ARGV_WORDS
            && words
                .iter()
                .map(|word| word.byte_range().len())
                .sum::<usize>()
                <= MAX_BASH_REGION_ARGV_BYTES;
        self.regions.complete &= bounded && !hides_commands;
        let argv = (bounded && !hides_words)
            .then(|| {
                words
                    .iter()
                    .map(|word| decode_static_word(&self.source[word.byte_range()]))
                    .collect()
            })
            .flatten();
        let executable = node
            .child_by_field_name("name")
            .and_then(|name| decode_static_word(&self.source[name.byte_range()]));
        self.regions.commands.push(BashRegionCommand {
            region,
            span: BashSpan::of(node),
            executable,
            argv,
        });
    }

    /// A region command's words in source order, with those the grammar attaches to redirects
    /// written after the statements the command ends, and the source the tree leaves out there.
    fn region_words<'tree>(&self, command: Node<'tree>) -> (Vec<Node<'tree>>, Vec<&str>) {
        let mut words = Vec::new();
        let mut omitted = Vec::new();
        let mut node = command;
        loop {
            omitted.extend(self.omitted(node));
            for (index, child) in children(node).into_iter().enumerate() {
                match child.kind() {
                    "file_redirect" => {
                        words.extend(field_children(child, "destination").into_iter().skip(1));
                    }
                    "heredoc_redirect" => words.extend(field_children(child, "argument")),
                    _ if matches!(
                        node.field_name_for_child(index as u32),
                        Some("name" | "argument")
                    ) =>
                    {
                        words.push(child);
                        continue;
                    }
                    _ => continue,
                }
                omitted.extend(self.omitted(child));
            }
            match node.parent() {
                Some(parent) if ends(parent, node) => node = parent,
                _ => break,
            }
        }
        words.sort_by_key(Node::start_byte);
        (words, omitted)
    }

    /// The source between a node's children that is neither in the tree nor trivia.
    fn omitted(&self, node: Node<'_>) -> Vec<&str> {
        let mut offset = node.start_byte();
        let mut omitted = Vec::new();
        for (start, end) in children(node)
            .iter()
            .map(|child| (child.start_byte(), child.end_byte()))
            .chain([(node.end_byte(), node.end_byte())])
        {
            if let Some(text) = self.source.get(offset..start).filter(|text| !trivia(text)) {
                omitted.push(text);
            }
            offset = offset.max(end);
        }
        omitted
    }

    fn newlines(
        &mut self,
        start: usize,
        end: usize,
        owner: BashNodeId,
        operators: &mut Vec<BashOperator>,
    ) {
        let mut offset = start;
        while offset < end {
            if self.source.as_bytes()[offset..end].starts_with(b"\\\n") {
                offset += 2;
                continue;
            }
            if self.source.as_bytes()[offset] == b'\n' {
                let span = BashSpan {
                    start: offset,
                    end: offset + 1,
                };
                self.claim(
                    span.clone(),
                    owner,
                    BashCoverageKind::Operator(BashOperatorKind::Newline),
                );
                operators.push(BashOperator {
                    span,
                    kind: BashOperatorKind::Newline,
                });
            }
            offset += 1;
        }
    }

    fn operator(&mut self, node: Node<'_>, owner: BashNodeId) -> Option<BashOperator> {
        if node.is_named() {
            return None;
        }
        let kind = match node.kind() {
            ";" => BashOperatorKind::Semicolon,
            "\n" => BashOperatorKind::Newline,
            "&&" => BashOperatorKind::And,
            "||" => BashOperatorKind::Or,
            "|" => BashOperatorKind::Pipe,
            "|&" => BashOperatorKind::PipeStderr,
            "&" => BashOperatorKind::Background,
            "(" => BashOperatorKind::OpenSubshell,
            ")" => BashOperatorKind::CloseSubshell,
            "{" => BashOperatorKind::OpenBrace,
            "}" => BashOperatorKind::CloseBrace,
            _ => return None,
        };
        let span = BashSpan::of(node);
        self.claim(
            span.clone(),
            owner,
            BashCoverageKind::Operator(kind.clone()),
        );
        Some(BashOperator { span, kind })
    }

    fn command_parts(
        &mut self,
        node: Node<'_>,
        id: BashNodeId,
        command: &mut BashCommand,
    ) -> Result<(), BashDiagnosticKind> {
        if !self.gaps_are_trivia(node) {
            return Err(BashDiagnosticKind::SourceGap);
        }
        reject_substitutions(node)?;
        for (index, child) in children(node).into_iter().enumerate() {
            match child.kind() {
                "command" | "redirected_statement" if node.kind() == "redirected_statement" => {
                    self.command_parts(child, id, command)?
                }
                "command_name" => self.command_word(child, id, command),
                "variable_assignment" => {
                    command
                        .parts
                        .push(BashCommandPart::Assignment(command.assignments.len()));
                    command.assignments.push(self.assignment(child, id)?);
                }
                "file_redirect" => self.redirect(child, id, command)?,
                "herestring_redirect" => self.herestring(child, id, command)?,
                "comment" => self.claim(BashSpan::of(child), id, BashCoverageKind::Trivia),
                _ if node.field_name_for_child(index as u32) == Some("argument") => {
                    self.command_word(child, id, command)
                }
                _ => return Err(unsupported(child)),
            }
        }
        Ok(())
    }

    fn command_word(&mut self, node: Node<'_>, id: BashNodeId, command: &mut BashCommand) {
        self.claim(BashSpan::of(node), id, BashCoverageKind::Word);
        command
            .parts
            .push(BashCommandPart::Word(command.words.len()));
        command.words.push(self.word(node));
    }

    fn word(&self, node: Node<'_>) -> BashWord {
        let span = BashSpan::of(node);
        let source = &self.source[span.start..span.end];
        let literal = decode_static_word(source);
        let mut fragments = Vec::new();
        self.fragments(node, &mut fragments);
        BashWord {
            span,
            literal,
            fragments,
        }
    }

    fn fragments(&self, node: Node<'_>, fragments: &mut Vec<BashWordFragment>) {
        if matches!(node.kind(), "concatenation" | "command_name") && node.child_count() > 0 {
            for child in children(node) {
                self.fragments(child, fragments);
            }
            return;
        }
        let span = BashSpan::of(node);
        let source = &self.source[span.start..span.end];
        let quoting = if source.starts_with("$'") {
            BashQuoting::AnsiC
        } else if source.starts_with("$\"") {
            BashQuoting::Translated
        } else if source.starts_with('\'') {
            BashQuoting::Single
        } else if source.starts_with('"') {
            BashQuoting::Double
        } else {
            BashQuoting::Unquoted
        };
        let value = decode_static_word(source).map_or_else(
            || BashFragmentValue::Dynamic(node.kind().to_owned()),
            BashFragmentValue::Literal,
        );
        fragments.push(BashWordFragment {
            span,
            quoting,
            value,
        });
    }

    fn assignment(
        &mut self,
        node: Node<'_>,
        id: BashNodeId,
    ) -> Result<BashAssignment, BashDiagnosticKind> {
        reject_substitutions(node)?;
        let Some(name) = node.child_by_field_name("name") else {
            return Err(unsupported(node));
        };
        if name.kind() != "variable_name" || !self.gaps_are_trivia(node) {
            return Err(unsupported(node));
        }
        let direct = children(node);
        if !direct.iter().any(|child| child.kind() == "=") {
            return Err(unsupported(node));
        }
        let value = if let Some(value) = node
            .child_by_field_name("value")
            .filter(|value| !value.byte_range().is_empty())
        {
            if value.kind() == "array" {
                return Err(unsupported(node));
            }
            self.word(value)
        } else {
            BashWord {
                span: BashSpan {
                    start: node.end_byte(),
                    end: node.end_byte(),
                },
                literal: Some(String::new()),
                fragments: Vec::new(),
            }
        };
        let span = BashSpan::of(node);
        self.claim(span.clone(), id, BashCoverageKind::Assignment);
        Ok(BashAssignment {
            span,
            name: self.source[name.byte_range()].to_owned(),
            value,
        })
    }

    fn redirect(
        &mut self,
        node: Node<'_>,
        id: BashNodeId,
        command: &mut BashCommand,
    ) -> Result<(), BashDiagnosticKind> {
        if !self.gaps_are_trivia(node) {
            return Err(BashDiagnosticKind::SourceGap);
        }
        let direct = children(node);
        let Some(operator) = direct.iter().find(|child| !child.is_named()) else {
            return Err(BashDiagnosticKind::AmbiguousRedirect);
        };
        let kind = match operator.kind() {
            "<" => BashRedirectKind::Read,
            ">" => BashRedirectKind::Write,
            ">>" => BashRedirectKind::Append,
            "&>" => BashRedirectKind::WriteBoth,
            "&>>" => BashRedirectKind::AppendBoth,
            "<&" => BashRedirectKind::DuplicateInput,
            ">&" => BashRedirectKind::DuplicateOutput,
            ">|" => BashRedirectKind::Clobber,
            "<&-" => BashRedirectKind::CloseInput,
            ">&-" => BashRedirectKind::CloseOutput,
            _ => return Err(BashDiagnosticKind::AmbiguousRedirect),
        };
        let descriptor = node.child_by_field_name("descriptor").map(BashSpan::of);
        let mut destinations = Vec::new();
        for (index, child) in direct.iter().enumerate() {
            match node.field_name_for_child(index as u32) {
                Some("destination") => destinations.push(*child),
                Some("descriptor") => {}
                _ if child == operator => {}
                _ => return Err(BashDiagnosticKind::AmbiguousRedirect),
            }
        }
        let close = matches!(
            kind,
            BashRedirectKind::CloseInput | BashRedirectKind::CloseOutput
        );
        if close && !destinations.is_empty() || !close && destinations.is_empty() {
            return Err(BashDiagnosticKind::AmbiguousRedirect);
        }
        let target = destinations.first().map(|target| self.word(*target));
        let span = BashSpan {
            start: node.start_byte(),
            end: target
                .as_ref()
                .map_or(operator.end_byte(), |word| word.span.end),
        };
        self.claim(span.clone(), id, BashCoverageKind::Redirect);
        command
            .parts
            .push(BashCommandPart::Redirect(command.redirects.len()));
        command.redirects.push(BashRedirect {
            span,
            operator: BashSpan::of(*operator),
            kind,
            descriptor,
            target,
        });
        for argument in destinations.into_iter().skip(1) {
            self.command_word(argument, id, command);
        }
        Ok(())
    }

    fn claim_unknown_with_payloads(&mut self, node: Node<'_>, id: BashNodeId) {
        let mut payloads = Vec::new();
        let mut stack = vec![node];
        while let Some(child) = stack.pop() {
            if matches!(child.kind(), "heredoc_body" | "herestring_redirect") {
                payloads.push(BashSpan::of(child));
            } else {
                stack.extend(children(child));
            }
        }
        payloads.sort_by_key(|span| span.start);
        let mut offset = node.start_byte();
        for payload in payloads {
            self.claim(
                BashSpan {
                    start: offset,
                    end: payload.start,
                },
                id,
                BashCoverageKind::Unknown,
            );
            offset = payload.end;
            self.claim(payload, id, BashCoverageKind::Payload);
        }
        self.claim(
            BashSpan {
                start: offset,
                end: node.end_byte(),
            },
            id,
            BashCoverageKind::Unknown,
        );
    }

    fn finish_coverage(&mut self) {
        self.coverage.sort_by_key(|coverage| coverage.span.start);
        let claimed = take(&mut self.coverage);
        let mut offset = 0;
        for coverage in claimed {
            if coverage.span.start < offset {
                self.diagnostics.push(BashDiagnostic {
                    span: coverage.span,
                    kind: BashDiagnosticKind::OverlappingCoverage,
                });
                continue;
            }
            self.cover_gap(offset, coverage.span.start);
            offset = coverage.span.end;
            self.coverage.push(coverage);
        }
        self.cover_gap(offset, self.source.len());
        if !self.is_complete() {
            for node in &mut self.nodes {
                if let BashNodeKind::Command { command } = &mut node.structure {
                    command.complete = false;
                }
            }
        }
    }

    fn cover_gap(&mut self, start: usize, end: usize) {
        if start == end {
            return;
        }
        let span = BashSpan { start, end };
        if trivia(&self.source[start..end]) {
            self.claim(span, self.root, BashCoverageKind::Trivia);
        } else {
            let reason = BashDiagnosticKind::SourceGap;
            self.diagnostics.push(BashDiagnostic {
                span: span.clone(),
                kind: reason.clone(),
            });
            let owner = self.add_node(span.clone(), BashNodeKind::Unknown { reason });
            self.claim(span, owner, BashCoverageKind::Unknown);
        }
    }
}

fn reject_substitutions(node: Node<'_>) -> Result<(), BashDiagnosticKind> {
    let mut stack = vec![node];
    while let Some(child) = stack.pop() {
        if matches!(
            child.kind(),
            "command_substitution" | "process_substitution" | "arithmetic_expansion"
        ) {
            return Err(unsupported(child));
        }
        stack.extend(children(child));
    }
    Ok(())
}

pub(crate) fn decode_static_word(word: &str) -> Option<String> {
    let mut decoded = String::new();
    let mut characters = word.chars();
    let mut quote = None;
    let mut has_word = false;
    while let Some(character) = characters.next() {
        match (quote, character) {
            (None, '\'') => {
                quote = Some('\'');
                has_word = true;
            }
            (None, '"') => {
                quote = Some('"');
                has_word = true;
            }
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), value) => decoded.push(value),
            (Some('"'), '$' | '`') => return None,
            (Some('"'), '\\') => {
                let escaped = characters.next()?;
                if matches!(escaped, '$' | '`' | '"' | '\\') {
                    decoded.push(escaped);
                } else if escaped != '\n' {
                    decoded.push('\\');
                    decoded.push(escaped);
                }
            }
            (Some('"'), value) => decoded.push(value),
            (None, '\\') => {
                let escaped = characters.next()?;
                if escaped != '\n' {
                    decoded.push(escaped);
                    has_word = true;
                }
            }
            (
                None,
                '$' | '`' | '*' | '?' | '[' | '{' | '~' | '(' | ')' | '<' | '>' | ';' | '&' | '|',
            ) => return None,
            (None, value) if value.is_whitespace() || value.is_control() => return None,
            (None, value) => {
                decoded.push(value);
                has_word = true;
            }
            _ => return None,
        }
    }
    (quote.is_none() && has_word).then_some(decoded)
}

fn retained_word_bytes(word: &BashWord) -> usize {
    word.literal
        .as_ref()
        .map_or(0, String::capacity)
        .saturating_add(
            word.fragments
                .capacity()
                .saturating_mul(size_of::<BashWordFragment>()),
        )
        .saturating_add(
            word.fragments
                .iter()
                .map(|fragment| match &fragment.value {
                    BashFragmentValue::Literal(value) | BashFragmentValue::Dynamic(value) => {
                        value.capacity()
                    }
                })
                .fold(0, usize::saturating_add),
        )
}

fn retained_assignments_bytes(assignments: &Vec<BashAssignment>) -> usize {
    assignments
        .capacity()
        .saturating_mul(size_of::<BashAssignment>())
        .saturating_add(
            assignments
                .iter()
                .map(|assignment| {
                    assignment
                        .name
                        .capacity()
                        .saturating_add(retained_word_bytes(&assignment.value))
                })
                .fold(0, usize::saturating_add),
        )
}

pub(super) fn retained_diagnostic_bytes(kind: &BashDiagnosticKind) -> usize {
    match kind {
        BashDiagnosticKind::UnsupportedSyntax(kind) => kind.capacity(),
        _ => 0,
    }
}

pub(super) fn retained_inventory_bytes(inventory: &BashRegionInventory) -> usize {
    inventory
        .commands
        .capacity()
        .saturating_mul(size_of::<BashRegionCommand>())
        .saturating_add(
            inventory
                .commands
                .iter()
                .map(|command| {
                    command
                        .executable
                        .as_ref()
                        .map_or(0, String::capacity)
                        .saturating_add(command.argv.as_ref().map_or(0, |argv| {
                            argv.capacity()
                                .saturating_mul(size_of::<String>())
                                .saturating_add(
                                    argv.iter()
                                        .map(String::capacity)
                                        .fold(0, usize::saturating_add),
                                )
                        }))
                })
                .fold(0, usize::saturating_add),
        )
}

pub(super) fn retained_node_bytes(node: &BashNode) -> usize {
    match &node.structure {
        BashNodeKind::Sequence { items, separators } => items
            .capacity()
            .saturating_mul(size_of::<BashNodeId>())
            .saturating_add(
                separators
                    .capacity()
                    .saturating_mul(size_of::<BashOperator>()),
            ),
        BashNodeKind::Pipeline {
            commands,
            operators,
        } => commands
            .capacity()
            .saturating_mul(size_of::<BashNodeId>())
            .saturating_add(
                operators
                    .capacity()
                    .saturating_mul(size_of::<BashOperator>()),
            ),
        BashNodeKind::Command { command } => command
            .words
            .capacity()
            .saturating_mul(size_of::<BashWord>())
            .saturating_add(
                command
                    .parts
                    .capacity()
                    .saturating_mul(size_of::<BashCommandPart>()),
            )
            .saturating_add(
                command
                    .redirects
                    .capacity()
                    .saturating_mul(size_of::<BashRedirect>()),
            )
            .saturating_add(
                command
                    .payloads
                    .capacity()
                    .saturating_mul(size_of::<BashPayload>()),
            )
            .saturating_add(
                command
                    .words
                    .iter()
                    .map(retained_word_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(retained_assignments_bytes(&command.assignments))
            .saturating_add(
                command
                    .redirects
                    .iter()
                    .filter_map(|redirect| redirect.target.as_ref())
                    .map(retained_word_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                command
                    .payloads
                    .iter()
                    .filter_map(|payload| payload.literal.as_ref())
                    .map(String::capacity)
                    .fold(0, usize::saturating_add),
            ),
        BashNodeKind::Assignments { assignments } => retained_assignments_bytes(assignments),
        BashNodeKind::Unknown { reason } => retained_diagnostic_bytes(reason),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use crate::bash::{BashCoverageKind, BashDiagnosticKind, parse_bash};

    #[test]
    fn an_unclaimed_argument_becomes_unknown_instead_of_trivia() {
        let mut program = parse_bash("cat retained omitted").unwrap();
        let removed = program
            .coverage
            .iter()
            .position(|covered| program.text(&covered.span) == Some("omitted"))
            .unwrap();
        let span = program.coverage.remove(removed).span;
        program.finish_coverage();
        assert!(!program.is_complete());
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
                .any(|covered| covered.span == span && covered.role == BashCoverageKind::Unknown)
        );
        assert!(
            program
                .commands()
                .all(|(_, command)| command.static_argv().is_none())
        );
    }
}
