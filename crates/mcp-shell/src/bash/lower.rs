use std::mem::take;

use tree_sitter::Node;

use super::{
    BASH_ANALYSIS_VERSION, BASH_GRAMMAR_VERSION, BashAssignment, BashCommand, BashCommandPart,
    BashCoverage, BashCoverageKind, BashDiagnostic, BashDiagnosticKind, BashFragmentValue,
    BashNode, BashNodeId, BashNodeKind, BashOperator, BashOperatorKind, BashProgram, BashQuoting,
    BashRedirect, BashRedirectKind, BashSpan, BashWord, BashWordFragment,
};

pub(super) fn lower(source: &str, root: Node<'_>) -> BashProgram {
    let mut program = BashProgram {
        analysis_version: BASH_ANALYSIS_VERSION,
        grammar_version: BASH_GRAMMAR_VERSION,
        source: source.to_owned(),
        root: BashNodeId(0),
        nodes: Vec::new(),
        coverage: Vec::new(),
        diagnostics: Vec::new(),
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
            "command" | "redirected_statement" => {
                let mut command = BashCommand {
                    complete: true,
                    words: Vec::new(),
                    assignments: Vec::new(),
                    redirects: Vec::new(),
                    parts: Vec::new(),
                };
                self.command_parts(node, id, &mut command)?;
                if command.words.is_empty() {
                    return Err(unsupported(node));
                }
                command.parts.sort_by_key(|part| match part {
                    BashCommandPart::Word(index) => command.words[*index].span.start,
                    BashCommandPart::Assignment(index) => command.assignments[*index].span.start,
                    BashCommandPart::Redirect(index) => command.redirects[*index].span.start,
                });
                Ok(BashNodeKind::Command { command })
            }
            "list" | "pipeline" => {
                if !self.gaps_are_trivia(node) {
                    return Err(BashDiagnosticKind::SourceGap);
                }
                let mut commands = Vec::new();
                let mut operators = Vec::new();
                for child in children(node) {
                    if child.kind() == "comment" {
                        self.claim(BashSpan::of(child), id, BashCoverageKind::Trivia);
                    } else if let Some(operator) = self.operator(child, id) {
                        operators.push(operator);
                    } else if child.is_named() {
                        commands.push(self.lower_node(child));
                    } else {
                        return Err(unsupported(child));
                    }
                }
                if node.kind() == "list"
                    && commands.len() == 2
                    && operators.len() == 1
                    && matches!(
                        operators[0].kind,
                        BashOperatorKind::And | BashOperatorKind::Or
                    )
                {
                    Ok(BashNodeKind::AndOr {
                        left: commands[0],
                        operator: operators.remove(0),
                        right: commands[1],
                    })
                } else if node.kind() == "pipeline"
                    && commands.len() > 1
                    && operators.len() + 1 == commands.len()
                    && operators.iter().all(|operator| {
                        matches!(
                            operator.kind,
                            BashOperatorKind::Pipe | BashOperatorKind::PipeStderr
                        )
                    })
                {
                    Ok(BashNodeKind::Pipeline {
                        commands,
                        operators,
                    })
                } else {
                    Err(unsupported(node))
                }
            }
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
        if child.kind() != "heredoc_body" {
            stack.extend(children(child));
        }
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
