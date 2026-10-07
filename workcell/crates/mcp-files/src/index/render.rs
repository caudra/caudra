use std::collections::BTreeMap;

use super::{
    floor_char_boundary,
    model::{
        Child, ChildKind, Entry, EntryValue, LineRange, ParsedSkeleton, RawLineMetadata, Section,
    },
    traversal::{Context, ParseFailure},
};

pub(super) const FIELD_TRUNCATE_THRESHOLD: usize = 8;
const LINE_WRAP_THRESHOLD: usize = 120;
const MAX_IMPORT_EXPANSION_BYTES: usize = 256 * 1024;
const MAX_IMPORT_EXPANSIONS: usize = 1_024;
const MAX_IMPORT_SEGMENTS: usize = 512;
const TRUNCATED: &str = "[truncated]";

#[derive(Default)]
struct ImportTrie {
    children: BTreeMap<String, Self>,
    is_leaf: bool,
}

impl ImportTrie {
    fn insert(&mut self, segments: &[String]) {
        let mut node = self;
        for segment in segments.iter().take(MAX_IMPORT_SEGMENTS) {
            node = node.children.entry(segment.clone()).or_default();
        }
        if segments.len() > MAX_IMPORT_SEGMENTS {
            node = node.children.entry(TRUNCATED.to_owned()).or_default();
        }
        node.is_leaf = true;
    }

    fn render_children(
        &self,
        separator: &str,
        context: &Context<'_>,
    ) -> Result<Vec<String>, ParseFailure> {
        let mut rendered = Vec::new();
        for (segment, node) in &self.children {
            context.check()?;
            rendered.extend(node.render_node(segment, separator, context)?);
        }
        Ok(rendered)
    }

    fn render_node(
        &self,
        segment: &str,
        separator: &str,
        context: &Context<'_>,
    ) -> Result<Vec<String>, ParseFailure> {
        context.check()?;
        if self.children.is_empty() {
            return Ok(vec![segment.to_owned()]);
        }
        let rendered = self.render_children(separator, context)?;
        Ok(if self.is_leaf {
            let mut output = vec![segment.to_owned()];
            output.extend(
                rendered
                    .into_iter()
                    .map(|item| format!("{segment}{separator}{item}")),
            );
            output
        } else if let [only] = rendered.as_slice() {
            vec![format!("{segment}{separator}{only}")]
        } else {
            vec![format!("{segment}{separator}{{{}}}", rendered.join(", "))]
        })
    }
}

pub(super) fn compact_whitespace(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut whitespace = false;
    for character in value.chars() {
        if character.is_ascii_whitespace() {
            if !whitespace {
                output.push(' ');
            }
            whitespace = true;
        } else {
            whitespace = false;
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;

    use super::{
        ImportTrie, MAX_IMPORT_EXPANSIONS, MAX_IMPORT_SEGMENTS, compact_whitespace, expand_import,
    };
    use crate::index::traversal::{Context, ExtractionGuard, ParseFailure};

    #[test]
    fn compact_whitespace_preserves_non_ascii_space() {
        assert_eq!(compact_whitespace("a\u{a0}\u{a0}b\n c"), "a\u{a0}\u{a0}b c");
    }

    #[test]
    fn import_rendering_bounds_adversarial_depth() {
        let segments = (0..MAX_IMPORT_SEGMENTS * 4)
            .map(|index| index.to_string())
            .collect::<Vec<_>>();
        let mut trie = ImportTrie::default();
        trie.insert(&segments);
        let guard = ExtractionGuard::new(
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
        );
        let context = Context::new("", &guard);

        let rendered = trie.render_children(".", &context).expect("render");

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].ends_with(".[truncated]"));
    }

    #[test]
    fn import_expansion_is_bounded_and_cancellable() {
        let items = (0..MAX_IMPORT_EXPANSIONS * 2)
            .map(|index| format!("item{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("root::{{{items}}}");
        let token = CancellationToken::new();
        let guard = ExtractionGuard::new(token.clone(), Instant::now() + Duration::from_secs(1));
        let context = Context::new("", &guard);

        let expanded = expand_import(&source, "::", &context).expect("expansion");

        assert_eq!(expanded.len(), MAX_IMPORT_EXPANSIONS);
        assert_eq!(expanded.last().unwrap(), &["[truncated]"]);
        token.cancel();
        assert!(matches!(
            expand_import("root::item", "::", &context),
            Err(ParseFailure::Cancelled)
        ));
    }
}

pub(super) fn prefixed(prefix: &str, rest: impl AsRef<str>) -> String {
    if prefix.is_empty() {
        rest.as_ref().to_owned()
    } else {
        format!("{prefix} {}", rest.as_ref())
    }
}

pub(super) fn truncate(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let boundary = floor_char_boundary(value, maximum.saturating_sub(TRUNCATED.len()));
    let prefix = &value[..boundary];
    if prefix.contains('\n') {
        format!("{prefix}\n{TRUNCATED}")
    } else {
        format!("{prefix}{TRUNCATED}")
    }
}

pub(super) fn truncated_message(total: usize) -> String {
    format!(
        "[{} more truncated]",
        total.saturating_sub(FIELD_TRUNCATE_THRESHOLD)
    )
}

pub(super) fn ranged(body: impl Into<String>, range: LineRange) -> Child {
    Child::Ranged {
        body: body.into(),
        range,
    }
}

pub(super) fn format_range(range: LineRange) -> String {
    if range.start == range.end {
        format!("[{}]", range.start)
    } else {
        format!("[{}-{}]", range.start, range.end)
    }
}

pub(super) fn expand_import(
    text: &str,
    separator: &str,
    context: &Context<'_>,
) -> Result<Vec<Vec<String>>, ParseFailure> {
    if text.len() > MAX_IMPORT_EXPANSION_BYTES {
        return Ok(vec![vec![TRUNCATED.to_owned()]]);
    }
    let mut output = Vec::new();
    let mut stack = vec![(Vec::new(), text.trim().to_owned())];
    let mut expansion_bytes = text.len();
    let mut truncated = false;
    while let Some((prefix, remaining)) = stack.pop() {
        context.check()?;
        if remaining.is_empty() {
            if !prefix.is_empty() {
                output.push(prefix);
            }
            continue;
        }
        let Some(position) = find_separator(&remaining, separator, context)? else {
            let mut path = prefix;
            path.push(remaining);
            output.push(path);
            if output.len() == MAX_IMPORT_EXPANSIONS {
                truncated |= !stack.is_empty();
                break;
            }
            continue;
        };
        let mut next_prefix = prefix;
        next_prefix.push(remaining[..position].to_owned());
        let rest = &remaining[position + separator.len()..];
        if let Some(inner) = rest
            .strip_prefix('{')
            .and_then(|value| value.strip_suffix('}'))
        {
            for item in split_top_level(inner, context)?.into_iter().rev() {
                let expansion = next_prefix
                    .iter()
                    .map(String::len)
                    .sum::<usize>()
                    .saturating_add(item.len());
                if output.len().saturating_add(stack.len()) == MAX_IMPORT_EXPANSIONS
                    || expansion_bytes.saturating_add(expansion) > MAX_IMPORT_EXPANSION_BYTES
                {
                    truncated = true;
                    break;
                }
                expansion_bytes += expansion;
                stack.push((next_prefix.clone(), item));
            }
        } else {
            let expansion = next_prefix
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(rest.len());
            if expansion_bytes.saturating_add(expansion) > MAX_IMPORT_EXPANSION_BYTES {
                truncated = true;
                break;
            }
            expansion_bytes += expansion;
            stack.push((next_prefix, rest.to_owned()));
        }
    }
    if truncated {
        output.truncate(MAX_IMPORT_EXPANSIONS.saturating_sub(1));
        output.push(vec![TRUNCATED.to_owned()]);
    }
    Ok(output)
}

fn find_separator(
    text: &str,
    separator: &str,
    context: &Context<'_>,
) -> Result<Option<usize>, ParseFailure> {
    let mut depth = 0usize;
    for (index, character) in text.char_indices() {
        context.check()?;
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && text[index..].starts_with(separator) => return Ok(Some(index)),
            _ => {}
        }
    }
    Ok(None)
}

fn split_top_level(text: &str, context: &Context<'_>) -> Result<Vec<String>, ParseFailure> {
    let mut output = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, character) in text.char_indices() {
        context.check()?;
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                output.push(text[start..index].trim().to_owned());
                start = index + 1;
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        output.push(last.to_owned());
    }
    Ok(output)
}

pub(super) fn format_skeleton(
    entries: &[Entry],
    test_lines: &[usize],
    module_doc: Option<LineRange>,
    import_separator: &str,
    context: &Context<'_>,
) -> Result<ParsedSkeleton, ParseFailure> {
    context.check()?;
    let mut renderer = Renderer::default();
    if let Some(range) = module_doc {
        renderer.push_section("module doc: ", Some(range));
    }
    for section in Section::ALL {
        context.check()?;
        let items = entries
            .iter()
            .filter(|entry| entry.section == section)
            .collect::<Vec<_>>();
        if items.is_empty() {
            continue;
        }
        renderer.blank();
        match section {
            Section::Import => renderer.render_imports(&items, import_separator, context)?,
            Section::Module => renderer.render_modules(&items, context)?,
            Section::Heading => renderer.render_headings(&items, context)?,
            _ => {
                renderer.push_section(section.header(), None);
                for entry in items {
                    renderer.render_item(entry, 2, context)?;
                }
            }
        }
    }
    if let (Some(start), Some(end)) = (test_lines.iter().min(), test_lines.iter().max()) {
        renderer.blank();
        let range = LineRange {
            start: *start,
            end: *end,
        };
        renderer.push_section("tests: ", Some(range));
    }
    context.check()?;
    Ok(renderer.finish())
}

#[derive(Default)]
struct Renderer {
    lines: Vec<String>,
    metadata: Vec<RawLineMetadata>,
}

impl Renderer {
    fn push(&mut self, line: String, metadata: RawLineMetadata) {
        self.lines.push(line);
        self.metadata.push(metadata);
    }

    fn blank(&mut self) {
        if !self.lines.is_empty() {
            self.push(String::new(), RawLineMetadata::default());
        }
    }

    fn push_section(&mut self, label: &str, range: Option<LineRange>) {
        let range_text = range.map(format_range);
        self.push(
            format!("{label}{}", range_text.as_deref().unwrap_or_default()),
            RawLineMetadata {
                tag: Some("section"),
                body: range_text.as_ref().map(|_| label.to_owned()),
                range: range_text,
            },
        );
    }

    fn render_imports(
        &mut self,
        entries: &[&Entry],
        separator: &str,
        context: &Context<'_>,
    ) -> Result<(), ParseFailure> {
        self.push_section("imports: ", Some(items_range(entries)));
        let mut tries = BTreeMap::<String, ImportTrie>::new();
        for entry in entries {
            context.check()?;
            let EntryValue::Import { paths, keyword } = &entry.value else {
                continue;
            };
            let trie = tries
                .entry(keyword.as_deref().unwrap_or("import").to_owned())
                .or_default();
            for path in paths {
                trie.insert(path);
            }
        }
        for (keyword, trie) in tries {
            for line in trie.render_children(separator, context)? {
                context.check()?;
                let line = if keyword == "import" {
                    format!("  {line}")
                } else {
                    format!("  {keyword}: {line}")
                };
                self.push(line, RawLineMetadata::default());
            }
        }
        Ok(())
    }

    fn render_modules(
        &mut self,
        entries: &[&Entry],
        context: &Context<'_>,
    ) -> Result<(), ParseFailure> {
        self.push_section("mod: ", Some(items_range(entries)));
        let names = entries
            .iter()
            .map(|entry| entry.text().to_owned())
            .collect::<Vec<_>>();
        for line in wrap_csv(&names, "  ") {
            context.check()?;
            self.push(line, RawLineMetadata::default());
        }
        Ok(())
    }

    fn render_headings(
        &mut self,
        entries: &[&Entry],
        context: &Context<'_>,
    ) -> Result<(), ParseFailure> {
        self.push_section("headings:", None);
        for entry in entries {
            context.check()?;
            let body = format!("  {}", entry.text());
            let range = format_range(entry.range);
            self.push(
                format!("{body} {range}"),
                RawLineMetadata {
                    body: Some(body),
                    range: Some(range),
                    ..RawLineMetadata::default()
                },
            );
        }
        Ok(())
    }

    fn render_item(
        &mut self,
        entry: &Entry,
        indent: usize,
        context: &Context<'_>,
    ) -> Result<(), ParseFailure> {
        context.check()?;
        let EntryValue::Item(item) = &entry.value else {
            return Ok(());
        };
        let prefix = " ".repeat(indent);
        for attr in &item.attrs {
            context.check()?;
            self.push(format!("{prefix}{attr}"), RawLineMetadata::default());
        }
        self.emit_ranged(format!("{prefix}{}", item.text), entry.range, context)?;
        if item.child_kind == ChildKind::Brief && !item.children.is_empty() {
            self.render_brief_children(&item.children, &prefix, context)?;
        } else {
            for child in &item.children {
                context.check()?;
                match child {
                    Child::Ranged { body, range } => {
                        self.emit_ranged(format!("{prefix}  {body}"), *range, context)?;
                    }
                    Child::Entry(entry) => self.render_item(entry, indent + 2, context)?,
                    Child::Text(text) => self.push(
                        format!("{prefix}  {text}"),
                        RawLineMetadata {
                            tag: ends_with_truncation(text).then_some("dim"),
                            ..RawLineMetadata::default()
                        },
                    ),
                }
            }
        }
        Ok(())
    }

    fn render_brief_children(
        &mut self,
        children: &[Child],
        prefix: &str,
        context: &Context<'_>,
    ) -> Result<(), ParseFailure> {
        context.check()?;
        let has_truncation = matches!(
            children.last(),
            Some(Child::Text(text)) if ends_with_truncation(text)
        );
        let content_end = children.len() - usize::from(has_truncation);
        let values = children[..content_end]
            .iter()
            .map(|child| match child {
                Child::Ranged { body, range } => format!("{body} {}", format_range(*range)),
                Child::Entry(entry) => entry.text().to_owned(),
                Child::Text(text) => text.clone(),
            })
            .collect::<Vec<_>>();
        for line in wrap_csv(&values, &format!("{prefix}  ")) {
            context.check()?;
            self.push(
                line,
                RawLineMetadata {
                    tag: Some("dim"),
                    ..RawLineMetadata::default()
                },
            );
        }
        if let Some(Child::Text(last)) = children.last().filter(|_| has_truncation) {
            self.push(
                format!("{prefix}  {last}"),
                RawLineMetadata {
                    tag: Some("dim"),
                    ..RawLineMetadata::default()
                },
            );
        }
        Ok(())
    }

    fn emit_ranged(
        &mut self,
        body: String,
        range: LineRange,
        context: &Context<'_>,
    ) -> Result<(), ParseFailure> {
        let range = format_range(range);
        if body.contains('\n') {
            let leading = body
                .chars()
                .take_while(|character| character.is_whitespace())
                .collect::<String>();
            for (index, line) in body.split('\n').enumerate() {
                context.check()?;
                if index == 0 {
                    self.push(
                        format!("{line} {range}"),
                        RawLineMetadata {
                            body: Some(line.to_owned()),
                            range: Some(range.clone()),
                            ..RawLineMetadata::default()
                        },
                    );
                } else {
                    let line = format!("{leading}{line}");
                    self.push(
                        line.clone(),
                        RawLineMetadata {
                            tag: ends_with_truncation(&line).then_some("dim"),
                            ..RawLineMetadata::default()
                        },
                    );
                }
            }
        } else {
            let truncated = ends_with_truncation(&body);
            self.push(
                format!("{body} {range}"),
                RawLineMetadata {
                    tag: truncated.then_some("dim"),
                    body: (!truncated).then_some(body),
                    range: (!truncated).then_some(range),
                },
            );
        }
        Ok(())
    }

    fn finish(self) -> ParsedSkeleton {
        ParsedSkeleton {
            skeleton: if self.lines.is_empty() {
                String::new()
            } else {
                format!("{}\n", self.lines.join("\n"))
            },
            metadata: self.metadata,
            parse_error: false,
        }
    }
}

fn items_range(entries: &[&Entry]) -> LineRange {
    LineRange {
        start: entries
            .iter()
            .map(|entry| entry.range.start)
            .min()
            .unwrap_or(1),
        end: entries
            .iter()
            .map(|entry| entry.range.end)
            .max()
            .unwrap_or(1),
    }
}

fn wrap_csv(items: &[String], indent: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = indent.to_owned();
    for (index, item) in items.iter().enumerate() {
        let addition = if index == 0 {
            item.clone()
        } else {
            format!(", {item}")
        };
        if index > 0 && current.len() + addition.len() > LINE_WRAP_THRESHOLD {
            output.push(current);
            current = format!("{indent}{item}");
        } else {
            current.push_str(&addition);
        }
    }
    if current.chars().any(|character| !character.is_whitespace()) {
        output.push(current);
    }
    output
}

fn ends_with_truncation(value: &str) -> bool {
    value.ends_with(TRUNCATED) || (value.starts_with('[') && value.ends_with(" more truncated]"))
}
