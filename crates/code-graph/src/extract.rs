//! Turns one parsed file into definitions and references.
//!
//! Extraction is query-driven. The tags query names what it found; this module gives each capture a
//! span, a name, an enclosing definition, and structural metrics. It never walks a language-specific
//! AST shape, so a grammar bump changes a query file rather than this code.
//!
//! The tree is dropped as soon as facts are extracted. A whole-repository parse that retained trees
//! would not fit any bound worth committing to.

use tree_sitter::{Node, Parser, QueryCursor, StreamingIterator as _, Tree};
use workcell_source_languages::{CaptureRole, Language, LanguageFamily, ReferenceKind, SymbolKind};

use crate::model::{ByteSpan, LineSpan, Metrics};

/// Extraction limits. Host-owned; never accepted from model input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtractLimits {
    /// Largest file admitted to a parse.
    pub max_source_bytes: usize,
    /// Deepest tree walk during metric collection. Bounds recursion on a pathological tree.
    pub max_depth: usize,
    /// Most definitions retained from one file.
    pub max_definitions_per_file: usize,
    /// Most references retained from one file.
    pub max_references_per_file: usize,
    /// Longest symbol name retained. A generated file can contain absurd identifiers.
    pub max_name_bytes: usize,
    /// Longest doc comment retained per definition.
    pub max_documentation_bytes: usize,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 4 * 1024 * 1024,
            max_depth: 256,
            max_definitions_per_file: 8_192,
            max_references_per_file: 32_768,
            max_name_bytes: 256,
            max_documentation_bytes: 512,
        }
    }
}

/// What one file contributed, before node ids are assigned.
#[derive(Debug, Default)]
pub struct FileFacts {
    pub definitions: Vec<RawDefinition>,
    pub references: Vec<RawReference>,
    pub parse_error: bool,
    pub definitions_truncated: bool,
    pub references_truncated: bool,
}

/// A definition before it has a node id.
#[derive(Clone, Debug)]
pub struct RawDefinition {
    pub name: String,
    pub kind: SymbolKind,
    pub name_start: usize,
    pub span: ByteSpan,
    pub lines: LineSpan,
    pub metrics: Metrics,
    pub test_scope: bool,
    pub documentation: Option<String>,
}

/// A reference before its enclosing definition has a node id.
#[derive(Clone, Debug)]
pub struct RawReference {
    pub name: String,
    pub kind: ReferenceKind,
    pub qualifier: Option<String>,
    pub byte: usize,
    pub line: usize,
    /// Index into this file's definition list, resolved to a `NodeId` once ids are assigned.
    pub enclosing: Option<usize>,
}

/// Parses one file and extracts its facts.
///
/// Returns `None` when the parser did not produce a tree, which the caller records as a skip rather
/// than a silent omission.
#[must_use]
pub fn extract(source: &str, language: Language, limits: ExtractLimits) -> Option<FileFacts> {
    let mut parser = Parser::new();
    parser.set_language(&language.grammar()).ok()?;
    let tree = parser.parse(source, None)?;
    Some(extract_from_tree(&tree, source, language, limits))
}

fn extract_from_tree(
    tree: &Tree,
    source: &str,
    language: Language,
    limits: ExtractLimits,
) -> FileFacts {
    let mut facts = FileFacts {
        parse_error: tree.root_node().has_error(),
        ..FileFacts::default()
    };
    let bytes = source.as_bytes();
    let query = language.tags_query();
    let capture_names = query.capture_names();
    let emits_calls = language.family() == LanguageFamily::Code;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);

    // Collected first, sorted second. A query yields matches in tree order per pattern, not in
    // document order across patterns, so nothing downstream may assume ordering here.
    let mut definitions: Vec<(RawDefinition, ByteSpan)> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    while let Some(matched) = matches.next() {
        let mut anchor: Option<(CaptureRole, Node<'_>)> = None;
        let mut name_node: Option<Node<'_>> = None;
        let mut qualifier_node: Option<Node<'_>> = None;
        let mut doc_node: Option<Node<'_>> = None;

        for capture in matched.captures() {
            match CaptureRole::classify(capture_names[capture.index as usize]) {
                role @ (CaptureRole::Definition(_) | CaptureRole::Reference(_)) => {
                    anchor = Some((role, capture.node));
                }
                CaptureRole::Name => name_node = Some(capture.node),
                CaptureRole::Qualifier => qualifier_node = Some(capture.node),
                CaptureRole::Doc => doc_node = Some(capture.node),
                CaptureRole::Ignored => {}
            }
        }

        let (Some((role, node)), Some(name_node)) = (anchor, name_node) else {
            continue;
        };
        let Some(name) = text_of(name_node, bytes, limits.max_name_bytes) else {
            continue;
        };

        match role {
            CaptureRole::Definition(kind) => {
                if definitions.len() >= limits.max_definitions_per_file {
                    facts.definitions_truncated = true;
                    continue;
                }
                let span = ByteSpan {
                    start: node.start_byte(),
                    end: node.end_byte(),
                };
                let metrics = measure(node, source, limits);
                definitions.push((
                    RawDefinition {
                        name,
                        kind,
                        name_start: name_node.start_byte(),
                        span,
                        lines: LineSpan {
                            start: node.start_position().row + 1,
                            end: node.end_position().row + 1,
                        },
                        metrics,
                        test_scope: in_test_scope(node, bytes),
                        documentation: doc_node
                            .and_then(|node| text_of(node, bytes, limits.max_documentation_bytes))
                            .map(|text| clean_documentation(&text)),
                    },
                    span,
                ));
            }
            CaptureRole::Reference(kind) => {
                // A config or prose lane never contributes a call edge. The query files are written
                // not to emit one, and this is the second gate: a future query edit cannot
                // manufacture control flow through a YAML key by accident.
                if kind == ReferenceKind::Call && !emits_calls {
                    continue;
                }
                if references.len() >= limits.max_references_per_file {
                    facts.references_truncated = true;
                    continue;
                }
                references.push(RawReference {
                    name,
                    kind,
                    qualifier: qualifier_node
                        .and_then(|node| text_of(node, bytes, limits.max_name_bytes)),
                    byte: node.start_byte(),
                    line: node.start_position().row + 1,
                    enclosing: None,
                });
            }
            _ => {}
        }
    }

    let definitions = dedup(definitions);
    attribute(&definitions, &mut references);
    facts.definitions = definitions
        .into_iter()
        .map(|(definition, _)| definition)
        .collect();
    facts.references = references;
    facts
}

/// Collapses captures that describe the same definition.
///
/// Identity is `(name token start, name)`. One definition routinely matches several patterns: a
/// method inside an `impl` matches both the method and the general function pattern. Keeping the
/// higher `SymbolKind` is what makes it record as a method, and keeping the *widest* span is what
/// preserves the whole body when one pattern captured only the signature.
///
/// The result is sorted by `(span.start, name_start)`, which is the order node ids are assigned in.
fn dedup(mut captured: Vec<(RawDefinition, ByteSpan)>) -> Vec<(RawDefinition, ByteSpan)> {
    captured.sort_by(|left, right| {
        (left.0.name_start, left.0.span.start, &left.0.name).cmp(&(
            right.0.name_start,
            right.0.span.start,
            &right.0.name,
        ))
    });

    let mut merged: Vec<(RawDefinition, ByteSpan)> = Vec::with_capacity(captured.len());
    for (definition, span) in captured {
        match merged.last_mut() {
            Some((previous, previous_span))
                if previous.name_start == definition.name_start
                    && previous.name == definition.name =>
            {
                if definition.kind > previous.kind {
                    previous.kind = definition.kind;
                }
                if span.len() > previous_span.len() {
                    *previous_span = span;
                    previous.span = span;
                    previous.lines = definition.lines;
                    previous.metrics = definition.metrics;
                }
                previous.test_scope |= definition.test_scope;
                if previous.documentation.is_none() {
                    previous.documentation = definition.documentation;
                }
            }
            _ => merged.push((definition, span)),
        }
    }

    merged.sort_by_key(|(definition, _)| (definition.span.start, definition.name_start));
    merged
}

/// Assigns each reference the innermost definition whose span contains it.
///
/// A sweep over spans sorted by start, with a stack of definitions still open at the current
/// offset. This is the step ripwire's span bug corrupted: when every method in a block shares the
/// block's span, the stack cannot distinguish them and every call in the block is attributed to
/// whichever one happens to sit on top.
fn attribute(definitions: &[(RawDefinition, ByteSpan)], references: &mut [RawReference]) {
    if definitions.is_empty() {
        return;
    }
    let mut order: Vec<usize> = (0..references.len()).collect();
    order.sort_by_key(|&index| references[index].byte);

    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0usize;
    for index in order {
        let offset = references[index].byte;
        while next < definitions.len() && definitions[next].1.start <= offset {
            stack.push(next);
            next += 1;
        }
        // Drop definitions that closed before this offset. Retain order so the innermost still-open
        // definition is the last entry.
        stack.retain(|&candidate| definitions[candidate].1.end > offset);
        references[index].enclosing = stack
            .iter()
            .rev()
            .find(|&&candidate| {
                definitions[candidate].1.contains(ByteSpan {
                    start: offset,
                    end: offset,
                })
            })
            .copied();
    }
}

fn text_of(node: Node<'_>, bytes: &[u8], maximum: usize) -> Option<String> {
    let text = node.utf8_text(bytes).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= maximum {
        return Some(trimmed.to_owned());
    }
    let mut boundary = maximum;
    while boundary > 0 && !trimmed.is_char_boundary(boundary) {
        boundary -= 1;
    }
    Some(trimmed[..boundary].to_owned())
}

/// Strips comment markers from a doc comment so the renderer does not have to.
fn clean_documentation(raw: &str) -> String {
    let mut lines = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        let line = line
            .strip_prefix("///")
            .or_else(|| line.strip_prefix("//!"))
            .or_else(|| line.strip_prefix("//"))
            .or_else(|| line.strip_prefix("/**"))
            .or_else(|| line.strip_prefix("*/"))
            .or_else(|| line.strip_prefix('*'))
            .or_else(|| line.strip_prefix("/*"))
            .or_else(|| line.strip_prefix('#'))
            .unwrap_or(line);
        let line = line.trim();
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines.join(" ")
}

/// Node kinds that introduce a branch, for cyclomatic complexity.
///
/// Matched by kind substring rather than an exact per-language list. The alternative is 34 tables
/// that drift apart; this is deliberately approximate and the metric is labelled as structural
/// rather than exact.
fn is_branch(kind: &str) -> bool {
    const BRANCHES: &[&str] = &[
        "if_statement",
        "if_expression",
        "if_let_expression",
        "elif_clause",
        "else_clause",
        "while_statement",
        "while_expression",
        "for_statement",
        "for_expression",
        "for_in_statement",
        "foreach_statement",
        "loop_expression",
        "do_statement",
        "switch_statement",
        "switch_expression",
        "match_expression",
        "match_arm",
        "case_clause",
        "case_statement",
        "when_entry",
        "catch_clause",
        "rescue",
        "except_clause",
        "conditional_expression",
        "ternary_expression",
        "guard_statement",
        "and",
        "or",
    ];
    BRANCHES.contains(&kind)
}

fn is_nesting(kind: &str) -> bool {
    kind.ends_with("_block")
        || kind.ends_with("_body")
        || matches!(kind, "block" | "compound_statement" | "statement_block")
}

/// Collects structural metrics for one definition body.
///
/// Bounded by `max_depth`. A tree deeper than that stops contributing rather than recursing, and
/// the metric is a floor for that definition like every other count in this crate.
fn measure(node: Node<'_>, source: &str, limits: ExtractLimits) -> Metrics {
    let mut metrics = Metrics {
        complexity: 1,
        lines: u32::try_from(
            node.end_position()
                .row
                .saturating_sub(node.start_position().row)
                + 1,
        )
        .unwrap_or(u32::MAX),
        parameters: 0,
        max_nesting: 0,
    };

    if let Some(parameters) = node
        .child_by_field_name("parameters")
        .or_else(|| node.child_by_field_name("parameter_list"))
    {
        metrics.parameters = u32::try_from(parameters.named_child_count()).unwrap_or(u32::MAX);
    }

    let _ = source;
    let mut stack = vec![(node, 0usize, 0u32)];
    while let Some((current, depth, nesting)) = stack.pop() {
        if depth >= limits.max_depth {
            continue;
        }
        let kind = current.kind();
        if depth > 0 && is_branch(kind) {
            metrics.complexity = metrics.complexity.saturating_add(1);
        }
        let nesting = if depth > 0 && is_nesting(kind) {
            let deeper = nesting.saturating_add(1);
            metrics.max_nesting = metrics.max_nesting.max(deeper);
            deeper
        } else {
            nesting
        };
        let mut walker = current.walk();
        for child in current.named_children(&mut walker) {
            stack.push((child, depth + 1, nesting));
        }
    }
    metrics
}

/// Whether a definition sits inside test scaffolding, decided syntactically.
///
/// Path rules alone miss a `#[cfg(test)] mod tests` inside a production file and misclassify a
/// production helper that happens to live under `tests/`. Walking the ancestors costs one pass and
/// answers the question the caller actually has: will excluding tests remove this symbol.
fn in_test_scope(node: Node<'_>, bytes: &[u8]) -> bool {
    const MARKERS: &[&str] = &[
        "#[cfg(test)]",
        "#[test]",
        "#[tokio::test]",
        "@Test",
        "[Test]",
        "[Fact]",
        "[Theory]",
        "@pytest",
        "func Test",
    ];
    let mut current = Some(node);
    let mut hops = 0;
    while let Some(scope) = current {
        if hops > 64 {
            break;
        }
        hops += 1;
        // A test block's own header is short; sampling its opening bytes avoids reading a whole
        // module body to answer a boolean.
        let start = scope.start_byte();
        let end = start.saturating_add(96).min(bytes.len());
        if start < end
            && let Ok(header) = std::str::from_utf8(&bytes[start..end])
        {
            let header = header.trim_start();
            if MARKERS.iter().any(|marker| header.starts_with(marker))
                || header.starts_with("mod tests")
                || header.starts_with("describe(")
                || header.starts_with("class Test")
            {
                return true;
            }
        }
        // A preceding sibling carries the attribute in languages that attach it separately.
        if let Some(previous) = scope.prev_sibling() {
            let start = previous.start_byte();
            let end = previous.end_byte().min(start.saturating_add(32));
            if start < end
                && let Ok(text) = std::str::from_utf8(&bytes[start..end])
                && MARKERS
                    .iter()
                    .any(|marker| text.trim_start().starts_with(marker))
            {
                return true;
            }
        }
        current = scope.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str, language: Language) -> FileFacts {
        extract(source, language, ExtractLimits::default()).expect("parses")
    }

    #[test]
    fn attributes_a_call_to_the_method_that_contains_it() {
        // The regression ripwire paid for: when the method span is the whole impl block, this call
        // is attributed to an arbitrary sibling and `helper` appears to call itself.
        let source = "
struct Widget;
impl Widget {
    fn bump(&self) -> u32 { Self::helper() }
    fn helper() -> u32 { 1 }
}
";
        let facts = facts(source, Language::Rust);
        let bump = facts
            .definitions
            .iter()
            .position(|definition| definition.name == "bump")
            .expect("bump");
        let call = facts
            .references
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("call to helper");
        assert_eq!(call.enclosing, Some(bump));
    }

    #[test]
    fn a_file_scope_reference_has_no_enclosing_definition() {
        let source = "
import os
os.getcwd()

def later():
    pass
";
        let facts = facts(source, Language::Python);
        let call = facts
            .references
            .iter()
            .find(|reference| reference.name == "getcwd")
            .expect("call");
        assert_eq!(call.enclosing, None);
    }

    #[test]
    fn dedup_keeps_the_more_specific_kind_and_the_wider_span() {
        // `bump` matches both the method and the general function pattern in the Rust query.
        let source = "
struct Widget;
impl Widget {
    fn bump(&self) -> u32 { 1 }
}
";
        let facts = facts(source, Language::Rust);
        let bump: Vec<_> = facts
            .definitions
            .iter()
            .filter(|definition| definition.name == "bump")
            .collect();
        assert_eq!(bump.len(), 1, "duplicate captures were not merged");
        assert_eq!(bump[0].kind, SymbolKind::Method);
    }

    #[test]
    fn definitions_are_ordered_by_span_start() {
        let source = "
fn first() {}
fn second() {}
fn third() {}
";
        let facts = facts(source, Language::Rust);
        let starts: Vec<_> = facts
            .definitions
            .iter()
            .map(|definition| definition.span.start)
            .collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted);
    }

    #[test]
    fn config_lanes_never_yield_call_references() {
        let source = "
[package]
name = \"catalog\"
";
        let facts = facts(source, Language::Toml);
        assert!(
            facts
                .references
                .iter()
                .all(|reference| reference.kind != ReferenceKind::Call)
        );
    }

    #[test]
    fn test_scope_is_syntactic_not_path_based() {
        let source = "
fn production() {}

#[cfg(test)]
mod tests {
    #[test]
    fn checks_something() { production(); }
}
";
        let facts = facts(source, Language::Rust);
        let production = facts
            .definitions
            .iter()
            .find(|definition| definition.name == "production")
            .expect("production");
        let checker = facts
            .definitions
            .iter()
            .find(|definition| definition.name == "checks_something")
            .expect("test fn");
        assert!(!production.test_scope);
        assert!(checker.test_scope);
    }

    #[test]
    fn complexity_counts_branches_in_the_body() {
        let source = "
fn plain() { let _ = 1; }
fn branchy(value: u32) -> u32 {
    if value > 2 { return 1; }
    match value { 0 => 1, _ => 2 }
}
";
        let facts = facts(source, Language::Rust);
        let plain = facts
            .definitions
            .iter()
            .find(|definition| definition.name == "plain")
            .expect("plain");
        let branchy = facts
            .definitions
            .iter()
            .find(|definition| definition.name == "branchy")
            .expect("branchy");
        assert_eq!(plain.metrics.complexity, 1);
        assert!(branchy.metrics.complexity > plain.metrics.complexity);
        assert_eq!(branchy.metrics.parameters, 1);
    }

    #[test]
    fn extraction_is_deterministic_across_repeated_runs() {
        let source = include_str!("extract.rs");
        let first = facts(source, Language::Rust);
        let second = facts(source, Language::Rust);
        let names = |facts: &FileFacts| {
            facts
                .definitions
                .iter()
                .map(|definition| (definition.name.clone(), definition.span.start))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&first), names(&second));
    }

    #[test]
    fn limits_truncate_rather_than_fail() {
        let source = "fn a() {} fn b() {} fn c() {}";
        let limits = ExtractLimits {
            max_definitions_per_file: 1,
            ..ExtractLimits::default()
        };
        let facts = extract(source, Language::Rust, limits).expect("parses");
        assert!(facts.definitions_truncated);
        assert_eq!(facts.definitions.len(), 1);
    }
}
