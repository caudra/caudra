use tree_sitter::Node;

use crate::index::{
    model::{Child, Entry, LineRange, ParsedSkeleton},
    render::{FIELD_TRUNCATE_THRESHOLD, truncated_message},
    traversal::{Context, ParseFailure},
};

pub(super) type ExtractResult<T> = Result<T, ParseFailure>;
pub(super) type ExtractNodes = for<'tree, 'source> fn(
    Node<'tree>,
    &Context<'source>,
    &[Node<'tree>],
) -> ExtractResult<Vec<Entry>>;
pub(super) type NodePredicate = for<'tree, 'source> fn(Node<'tree>, &Context<'source>) -> bool;
pub(super) type TestPredicate =
    for<'tree, 'source> fn(Node<'tree>, &Context<'source>, &[Node<'tree>]) -> bool;

#[derive(Clone, Copy)]
pub(super) struct LanguageSpec {
    pub(super) import_separator: &'static str,
    pub(super) extract_nodes: ExtractNodes,
    pub(super) is_doc_comment: Option<NodePredicate>,
    pub(super) is_module_doc: Option<NodePredicate>,
    pub(super) is_attr: Option<NodePredicate>,
    pub(super) is_test_node: Option<TestPredicate>,
}

impl LanguageSpec {
    pub(super) const fn new(import_separator: &'static str, extract_nodes: ExtractNodes) -> Self {
        Self {
            import_separator,
            extract_nodes,
            is_doc_comment: None,
            is_module_doc: None,
            is_attr: None,
            is_test_node: None,
        }
    }
}

pub(super) fn extract_default(
    root: Node<'_>,
    context: &Context<'_>,
    spec: LanguageSpec,
) -> ExtractResult<ParsedSkeleton> {
    let mut entries = Vec::new();
    let mut test_lines = Vec::new();
    for child in context.children(root)? {
        if predicate(spec.is_attr, child, context) || predicate(spec.is_doc_comment, child, context)
        {
            continue;
        }
        let attrs = preceding_attrs(child, context, spec.is_attr)?;
        if spec
            .is_test_node
            .is_some_and(|is_test| is_test(child, context, &attrs))
        {
            test_lines.push(child.start_position().row + 1);
            continue;
        }
        let mut extracted = (spec.extract_nodes)(child, context, &attrs)?;
        if let (Some(first), Some(is_doc)) = (extracted.first_mut(), spec.is_doc_comment)
            && let Some(start) = preceding_doc_start(child, context, is_doc, spec.is_attr)?
        {
            first.range.start = first.range.start.min(start);
        }
        entries.extend(extracted);
    }
    let module_doc = detect_module_doc(root, context, spec.is_module_doc, spec.is_attr)?;
    crate::index::render::format_skeleton(
        &entries,
        &test_lines,
        module_doc,
        spec.import_separator,
        context,
    )
}

fn predicate(predicate: Option<NodePredicate>, node: Node<'_>, context: &Context<'_>) -> bool {
    predicate.is_some_and(|predicate| predicate(node, context))
}

fn preceding_attrs<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
    is_attr: Option<NodePredicate>,
) -> ExtractResult<Vec<Node<'tree>>> {
    let Some(is_attr) = is_attr else {
        return Ok(Vec::new());
    };
    let mut attrs = Vec::new();
    let mut previous = node.prev_sibling();
    while let Some(candidate) = previous {
        context.check()?;
        if !is_attr(candidate, context) {
            break;
        }
        attrs.push(candidate);
        previous = candidate.prev_sibling();
    }
    attrs.reverse();
    Ok(attrs)
}

fn preceding_doc_start(
    node: Node<'_>,
    context: &Context<'_>,
    is_doc: NodePredicate,
    is_attr: Option<NodePredicate>,
) -> ExtractResult<Option<usize>> {
    let mut earliest = None;
    let mut previous = node.prev_sibling();
    while let Some(candidate) = previous {
        context.check()?;
        if predicate(is_attr, candidate, context) {
            previous = candidate.prev_sibling();
        } else if is_doc(candidate, context) {
            earliest = Some(candidate.start_position().row + 1);
            previous = candidate.prev_sibling();
        } else {
            break;
        }
    }
    Ok(earliest)
}

fn detect_module_doc(
    root: Node<'_>,
    context: &Context<'_>,
    is_module_doc: Option<NodePredicate>,
    is_attr: Option<NodePredicate>,
) -> ExtractResult<Option<LineRange>> {
    let Some(is_module_doc) = is_module_doc else {
        return Ok(None);
    };
    let mut range = None;
    for child in context.children(root)? {
        if is_module_doc(child, context) {
            let end = child.end_position();
            let end = if end.column == 0 {
                end.row
            } else {
                end.row + 1
            };
            range = Some(LineRange {
                start: range.map_or(child.start_position().row + 1, |range: LineRange| {
                    range.start
                }),
                end,
            });
        } else if !predicate(is_attr, child, context) && !child.is_extra() {
            break;
        }
    }
    Ok(range)
}

pub(super) fn extract_enum_variants(
    body: Node<'_>,
    context: &Context<'_>,
    variant_kind: &str,
) -> ExtractResult<Vec<Child>> {
    let mut values = Vec::new();
    for child in context.children(body)? {
        if child.kind() == variant_kind {
            values.push(
                context
                    .field(child, "name")?
                    .map_or("_", |name| context.text(name))
                    .to_owned()
                    .into(),
            );
        }
    }
    Ok(values)
}

pub(super) fn extract_fields_truncated<F>(
    body: Node<'_>,
    context: &Context<'_>,
    field_kind: &str,
    mut format: F,
) -> ExtractResult<Vec<Child>>
where
    F: FnMut(Node<'_>, &Context<'_>) -> ExtractResult<String>,
{
    let mut fields = Vec::new();
    let mut total = 0usize;
    for child in context.children(body)? {
        if child.kind() == field_kind {
            total += 1;
            if total <= FIELD_TRUNCATE_THRESHOLD {
                fields.push(format(child, context)?.into());
            }
        }
    }
    if total > FIELD_TRUNCATE_THRESHOLD {
        fields.push(truncated_message(total).into());
    }
    Ok(fields)
}

pub(super) fn split_path(value: &str, separator: char) -> Vec<String> {
    value
        .split(separator)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn strip_delimited<'a>(value: &'a str, delimiter: &str) -> Option<&'a str> {
    value.strip_prefix(delimiter)?.strip_suffix(delimiter)
}

pub(super) fn trim_ascii_whitespace(value: &str) -> &str {
    value.trim_matches(|character: char| character.is_ascii_whitespace())
}

pub(super) fn strip_keyword_whitespace<'a>(value: &'a str, keyword: &str) -> &'a str {
    let Some(rest) = value.strip_prefix(keyword) else {
        return value;
    };
    let cleaned = rest.trim_start_matches(|character: char| character.is_ascii_whitespace());
    if cleaned.len() == rest.len() {
        value
    } else {
        cleaned
    }
}

pub(super) fn simple_import(node: Node<'_>, cleaned: &str, separator: char) -> Entry {
    Entry::import(node, vec![split_path(cleaned.trim(), separator)], None)
}

pub(super) fn plain_skeleton(skeleton: String) -> ParsedSkeleton {
    ParsedSkeleton {
        skeleton,
        metadata: Vec::new(),
        parse_error: false,
    }
}

pub(super) fn uppercase_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::{
        split_path, strip_delimited, strip_keyword_whitespace, trim_ascii_whitespace,
        uppercase_identifier,
    };

    #[test]
    fn shared_extractor_helpers_match_lua_semantics() {
        assert_eq!(split_path("..pkg.item", '.'), ["pkg", "item"]);
        assert!(uppercase_identifier("MAX_PORT"));
        assert!(!uppercase_identifier("HTTP2_PORT"));
        assert_eq!(strip_delimited(r#""pkg\"""#, "\""), Some(r#"pkg\""#));
        assert_eq!(strip_keyword_whitespace("import\tpkg", "import"), "pkg");
        assert_eq!(
            strip_keyword_whitespace("@import\"pkg\"", "@import"),
            "@import\"pkg\""
        );
        assert_eq!(trim_ascii_whitespace(" \tvalue\u{a0}"), "value\u{a0}");
    }
}
