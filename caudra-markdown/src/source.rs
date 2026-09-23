//! Markdown source coloured in place. Nothing is rendered away: every byte
//! keeps its position and only gains a style token, so a caller can show
//! exactly what was written with its structure picked out.

use std::ops::Range;
use std::ptr;
use std::sync::Arc;

use caudra_highlight::{Highlighter, TAB_SPACES, syntax_for_token, syntax_set};

use crate::render::{Line, LineKind, Span, StyleToken, coalesce_adjacent_spans};
use crate::{
    Block, BlockKind, Emphasis, LineBlock, Links, ParseMode, Source, SpanKind, parse,
    parse_inline_impl, table_cells,
};

/// Longest line or table cell whose inline markup is picked out, and longest
/// line a fenced block may hold and still be highlighted; anything longer stays
/// plain text. The inline scanners look ahead to the end of the line from every
/// opener, so their cost grows with the square of its length. 4 KiB still holds
/// the longest paragraph a model writes on one line.
const MAX_LINE_BYTES: usize = 4 * 1024;

/// One line per `\n`-separated source line, whose span texts concatenate to
/// exactly that line. A CRLF line keeps its `\r`, so nothing is lost for the
/// caller to escape or copy.
pub fn source_lines(text: &str) -> Vec<Line> {
    let mut painter = Painter::default();
    for block in parse(text) {
        painter.block(text, &block);
    }
    painter.into_lines(text)
}

/// A styled byte range of the source. Bytes no paint covers are plain text.
struct Paint {
    range: Range<usize>,
    style: StyleToken,
    emphasis: Emphasis,
    link: Option<Arc<str>>,
}

/// Paints in source order, and the kind of every line a block starts, both
/// cut into lines at the end.
#[derive(Default)]
struct Painter {
    paints: Vec<Paint>,
    kinds: Vec<(usize, LineKind)>,
}

impl Painter {
    fn block(&mut self, text: &str, block: &Block) {
        match block {
            Block::Lines(lines) => {
                for line in lines {
                    self.line(text, line);
                }
            }
            Block::Code {
                lang,
                code,
                source,
                code_start,
                ..
            } => {
                let source = widen(source);
                let body = *code_start as usize..*code_start as usize + code.len();
                self.claim(text, &source, LineKind::Code);
                self.paint(source.start..body.start, StyleToken::Syntax);
                if is_known_language(lang)
                    && code.split('\n').all(|line| line.len() <= MAX_LINE_BYTES)
                {
                    self.highlight(lang, code, body.start);
                }
                self.paint(body.end..source.end, StyleToken::Syntax);
            }
            Block::Table {
                header_end,
                row_sources,
                separator,
                ..
            } => {
                let (header, body) = row_sources.split_at((*header_end).min(row_sources.len()));
                for row in header {
                    self.table_row(text, widen(row), Emphasis::BOLD);
                }
                let separator = widen(separator);
                self.kinds.push((separator.start, LineKind::TableBorder));
                self.paint(separator, StyleToken::TableBorder);
                for row in body {
                    self.table_row(text, widen(row), Emphasis::default());
                }
            }
            Block::Math { source, .. } => {
                let source = widen(source);
                self.claim(text, &source, LineKind::Math);
                self.paint(source, StyleToken::Math);
            }
        }
    }

    fn line(&mut self, text: &str, block: &LineBlock) {
        let line = widen(&block.source);
        if line.is_empty() {
            return;
        }
        let content = block.inline_start as usize..block.inline_start as usize + block.inline.len();
        let plain = Emphasis::default();
        match block.kind {
            BlockKind::HorizontalRule => {
                self.kinds.push((line.start, LineKind::HorizontalRule));
                self.paint(line, StyleToken::HorizontalRule);
            }
            BlockKind::Heading(_) => {
                self.kinds.push((line.start, LineKind::Heading));
                self.paint(line.start..content.start, StyleToken::Heading);
                self.inline(text, content.clone(), StyleToken::Heading, plain);
                self.paint(content.end..line.end, StyleToken::Heading);
            }
            BlockKind::UnorderedListItem { .. } | BlockKind::OrderedListItem { .. } => {
                self.kinds.push((line.start, LineKind::ListItem));
                let indent = text[line.clone()]
                    .bytes()
                    .take_while(|&b| b == b' ')
                    .count();
                self.paint(line.start + indent..content.start, StyleToken::ListMarker);
                self.inline(text, content, StyleToken::Text, plain);
            }
            BlockKind::Paragraph => {
                self.kinds.push((line.start, LineKind::Paragraph));
                self.inline(text, content, StyleToken::Text, plain);
            }
        }
    }

    /// Spans cover exactly the text the renderer shows, so whatever lies
    /// between them is markup it drops.
    fn inline(&mut self, text: &str, content: Range<usize>, base: StyleToken, emphasis: Emphasis) {
        if content.len() > MAX_LINE_BYTES {
            return;
        }
        let mut cursor = content.start;
        for span in parse_inline_impl(
            &text[content.clone()],
            content.start as u32,
            emphasis,
            ParseMode::WithCode,
            Links::Verbatim,
        ) {
            let range = widen(&span.source.range);
            self.paint(cursor..range.start, StyleToken::Syntax);
            cursor = range.end;
            let style = match span.kind {
                SpanKind::Text => base.clone(),
                SpanKind::Code => StyleToken::InlineCode,
                SpanKind::Math => StyleToken::Math,
            };
            self.paints.push(Paint {
                range,
                style,
                emphasis: span.emphasis,
                link: span.link,
            });
        }
        self.paint(cursor..content.end, StyleToken::Syntax);
    }

    fn table_row(&mut self, text: &str, row: Range<usize>, emphasis: Emphasis) {
        self.kinds.push((row.start, LineKind::TableRow));
        let mut cursor = row.start;
        for (cell, _) in table_cells(&text[row.clone()]) {
            let cell = row.start + cell.start..row.start + cell.end;
            self.paint(cursor..cell.start, StyleToken::TableBorder);
            cursor = cell.end;
            self.inline(text, cell, StyleToken::Text, emphasis);
        }
        self.paint(cursor..row.end, StyleToken::TableBorder);
    }

    /// The highlighter expands tabs, so each segment is measured back in
    /// source bytes before it is laid over its line.
    fn highlight(&mut self, lang: &str, code: &str, start: usize) {
        let lines = code.split('\n');
        let highlighted = Highlighter::for_token(lang).highlight_lines(lines.clone());
        let mut line_start = start;
        for (line, segments) in lines.zip(highlighted) {
            let mut at = 0;
            for segment in &segments {
                let len = source_len(&line[at..], &segment.text);
                self.paint(
                    line_start + at..line_start + at + len,
                    StyleToken::from(segment),
                );
                at += len;
            }
            line_start += line.len() + 1;
        }
    }

    fn claim(&mut self, text: &str, range: &Range<usize>, kind: LineKind) {
        let mut start = range.start;
        for line in text[range.clone()].split('\n') {
            self.kinds.push((start, kind.clone()));
            start += line.len() + 1;
        }
    }

    fn paint(&mut self, range: Range<usize>, style: StyleToken) {
        if !range.is_empty() {
            self.paints.push(Paint {
                range,
                style,
                emphasis: Emphasis::default(),
                link: None,
            });
        }
    }

    fn into_lines(self, text: &str) -> Vec<Line> {
        let mut paints = self.paints.into_iter().peekable();
        let mut kinds = self.kinds.into_iter().peekable();
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split('\n') {
            let end = start + line.len();
            let mut spans = Vec::new();
            let mut at = start;
            while let Some(paint) = paints.peek()
                && paint.range.start < end
            {
                let from = paint.range.start.max(at);
                let to = paint.range.end.min(end);
                if from < to {
                    if at < from {
                        spans.push(span(text, at..from, StyleToken::Text, Emphasis::default()));
                    }
                    spans.push(
                        span(text, from..to, paint.style.clone(), paint.emphasis)
                            .with_link(paint.link.clone()),
                    );
                    at = to;
                }
                if paint.range.end > end {
                    break;
                }
                paints.next();
            }
            if at < end {
                spans.push(span(text, at..end, StyleToken::Text, Emphasis::default()));
            }
            coalesce_adjacent_spans(&mut spans);

            while kinds.next_if(|(offset, _)| *offset < start).is_some() {}
            let kind = match kinds.next_if(|(offset, _)| *offset == start) {
                Some((_, kind)) => kind,
                None if line.is_empty() => LineKind::Blank,
                None => LineKind::Paragraph,
            };
            lines.push(Line {
                kind,
                spans,
                source: Some(start as u32..end as u32),
            });
            start = end + 1;
        }
        lines
    }
}

fn span(text: &str, range: Range<usize>, style: StyleToken, emphasis: Emphasis) -> Span {
    let source = Source::verbatim(range.start as u32..range.end as u32);
    Span::sourced(&text[range], style, emphasis, source)
}

/// Bytes of `source` the highlighter showed as `shown`, having widened
/// every tab to [`TAB_SPACES`].
fn source_len(source: &str, shown: &str) -> usize {
    let mut width = 0;
    for (at, ch) in source.char_indices() {
        if width >= shown.len() {
            return at;
        }
        width += match ch {
            '\t' => TAB_SPACES.len(),
            _ => ch.len_utf8(),
        };
    }
    source.len()
}

/// A fence naming no language the highlighter knows keeps its body plain.
fn is_known_language(lang: &str) -> bool {
    !ptr::eq(
        syntax_for_token(lang),
        syntax_set().find_syntax_plain_text(),
    )
}

fn widen(range: &Range<u32>) -> Range<usize> {
    range.start as usize..range.end as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::SpanSource;
    use caudra_highlight::normalize_text;
    use test_case::test_case;

    const LINK_TARGET: &str = "https://example.com/a";
    const RUST: &str = "rust";
    const TABBED_RUST: &str = "\tlet s = \"\tx\";";
    const INLINE_CODE: &str = "`x`";
    const FILLER: &str = "x";

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|span| span.text.as_str()).collect()
    }

    fn styles(text: &str, line: usize) -> Vec<(String, StyleToken)> {
        source_lines(text)[line]
            .spans
            .iter()
            .map(|span| (span.text.clone(), span.style.clone()))
            .collect()
    }

    fn span_with_text(lines: &[Line], text: &str) -> Span {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text == text)
            .cloned()
            .unwrap_or_else(|| panic!("no span {text:?}"))
    }

    #[test_case("# Heading with **bold** and `code`  "; "heading")]
    #[test_case("Some **bold**, *italic*, _under_, ~~gone~~ and ***both***"; "emphasis")]
    #[test_case("Call `run()` or ``a ` tick`` or `unclosed"; "inline_code")]
    #[test_case("```rust\nfn main() {\n\tprintln!(\"hi\");\n}\n```"; "fenced_block_with_language")]
    #[test_case("```\nplain **not bold**\n\n```\nafter"; "fenced_block_without_language")]
    #[test_case("```\nx\n```tail"; "fence_closed_mid_line")]
    #[test_case("- one\n  - nested **deep**\n    1. ordered\n* star\n+ plus"; "nested_lists")]
    #[test_case("| a | `b|c` |\n| --- | :-: |\n| **x** | y \\| z |\n| tail"; "table")]
    #[test_case("[**bold** label](https://example.com \"title\") <https://a.b> https://c.d/e ![img](x.png)"; "links")]
    #[test_case("Inline $x^2$ and \\(y\\)\n$$\nE = mc^2\n$$\n\\[\nz\n\\]"; "math")]
    #[test_case("$$\nunterminated"; "unterminated_math")]
    #[test_case("<system-reminder>\nKeep going.\n</system-reminder>"; "system_reminder_tags")]
    #[test_case("# t\r\n\r\n- i\r\na\r\n**b**\r\n```rust\r\nlet x = 1;\r\n```\r\n| a |\r\n| - |\r\n"; "crlf")]
    #[test_case("\n\npara\n\n\n---\n\n"; "blank_lines")]
    #[test_case("text\n```python\ndef f():\n    return 1\n"; "unterminated_fence")]
    #[test_case("# Überschrift\n**fett** ñandú `código` 日本語 🎉\n| é | 表 |\n|---|---|\n| ü\u{3000} | ✓ |"; "non_ascii")]
    #[test_case(""; "empty")]
    fn spans_reproduce_each_source_line(text: &str) {
        let lines = source_lines(text);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, text.split('\n').collect::<Vec<_>>());
        for line in &lines {
            let range = line.source.clone().expect("line source");
            assert_eq!(
                line_text(line),
                text[range.start as usize..range.end as usize]
            );
            for span in &line.spans {
                let SpanSource::Range(Source {
                    range,
                    verbatim: true,
                }) = &span.source
                else {
                    panic!("span {:?} is not a verbatim slice", span.text);
                };
                assert_eq!(text[range.start as usize..range.end as usize], span.text);
            }
        }
    }

    #[test_case("## Title", 0, &[("## Title", StyleToken::Heading)]; "heading_includes_hashes")]
    #[test_case("# A `c`", 0, &[("# A ", StyleToken::Heading), ("`", StyleToken::Syntax), ("c", StyleToken::InlineCode), ("`", StyleToken::Syntax)]; "heading_keeps_inline_code")]
    #[test_case("**b** _i_", 0, &[("**", StyleToken::Syntax), ("b", StyleToken::Text), ("**", StyleToken::Syntax), (" ", StyleToken::Text), ("_", StyleToken::Syntax), ("i", StyleToken::Text), ("_", StyleToken::Syntax)]; "emphasis_delimiters")]
    #[test_case("2 * 3 = 6 and **open", 0, &[("2 * 3 = 6 and **open", StyleToken::Text)]; "unmatched_delimiters_stay_text")]
    #[test_case("`x`", 0, &[("`", StyleToken::Syntax), ("x", StyleToken::InlineCode), ("`", StyleToken::Syntax)]; "inline_code")]
    #[test_case("- a", 0, &[("- ", StyleToken::ListMarker), ("a", StyleToken::Text)]; "bullet_marker")]
    #[test_case("  12. b", 0, &[("  ", StyleToken::Text), ("12. ", StyleToken::ListMarker), ("b", StyleToken::Text)]; "nested_ordered_marker")]
    #[test_case("[a](https://x.y)", 0, &[("[", StyleToken::Syntax), ("a", StyleToken::Text), ("](https://x.y)", StyleToken::Syntax)]; "link_brackets_and_url")]
    #[test_case("<https://x.y>", 0, &[("<", StyleToken::Syntax), ("https://x.y", StyleToken::Text), (">", StyleToken::Syntax)]; "autolink_brackets")]
    #[test_case("| a | b |\n| --- | --- |", 0, &[("| ", StyleToken::TableBorder), ("a", StyleToken::Text), (" | ", StyleToken::TableBorder), ("b", StyleToken::Text), (" |", StyleToken::TableBorder)]; "table_pipes")]
    #[test_case("| a | b |\n| --- | --- |", 1, &[("| --- | --- |", StyleToken::TableBorder)]; "table_separator")]
    #[test_case("| `a|b` |\n| - |", 0, &[("| ", StyleToken::TableBorder), ("`", StyleToken::Syntax), ("a|b", StyleToken::InlineCode), ("`", StyleToken::Syntax), (" |", StyleToken::TableBorder)]; "table_pipe_inside_code")]
    #[test_case("* * *", 0, &[("* * *", StyleToken::HorizontalRule)]; "thematic_break")]
    #[test_case("a $x^2$ b", 0, &[("a ", StyleToken::Text), ("$x^2$", StyleToken::Math), (" b", StyleToken::Text)]; "inline_math")]
    #[test_case("$$\nx\n$$", 1, &[("x", StyleToken::Math)]; "display_math")]
    #[test_case("```rust\nx\n```", 0, &[("```rust", StyleToken::Syntax)]; "opening_fence")]
    #[test_case("```rust\nx\n```", 2, &[("```", StyleToken::Syntax)]; "closing_fence")]
    #[test_case("```\n**x**\n```", 1, &[("**x**", StyleToken::Text)]; "unlabelled_fence_body_stays_plain")]
    #[test_case("```nosuchlang\n`x`\n```", 1, &[("`x`", StyleToken::Text)]; "unknown_language_body_stays_plain")]
    #[test_case("```\nx\n```tail", 2, &[("```", StyleToken::Syntax), ("tail", StyleToken::Text)]; "fence_closed_mid_line")]
    #[test_case("<system-reminder>", 0, &[("<system-reminder>", StyleToken::Text)]; "system_reminder_tag")]
    fn construct_takes_its_token(text: &str, line: usize, expected: &[(&str, StyleToken)]) {
        let expected: Vec<(String, StyleToken)> = expected
            .iter()
            .map(|(text, style)| (text.to_string(), style.clone()))
            .collect();
        assert_eq!(styles(text, line), expected);
    }

    #[test_case("```rust\nlet x = 1;\n```"; "spaces")]
    #[test_case("```go\n\tx := \"é\"\t// tab\n```"; "tabs")]
    #[test_case("```rust\r\nlet x = 1;\r\n```"; "crlf")]
    fn known_language_body_is_highlighted(text: &str) {
        let body = &source_lines(text)[1];
        assert!(!body.spans.is_empty());
        assert!(
            body.spans
                .iter()
                .all(|span| matches!(span.style, StyleToken::Highlight { .. })),
            "{:?}",
            body.spans
        );
    }

    #[test_case("abc", "ab" => 2; "plain_prefix")]
    #[test_case("\tab", "  a" => 2; "tab_widened")]
    #[test_case("é\tz", "é  " => 3; "multibyte_before_tab")]
    #[test_case("ab", "" => 0; "empty_segment")]
    #[test_case("ab", "abc" => 2; "segment_past_line_end")]
    fn segment_measures_in_source_bytes(source: &str, shown: &str) -> usize {
        source_len(source, shown)
    }

    #[test]
    fn highlighted_segments_keep_their_source_bytes() {
        let mut painter = Painter::default();
        painter.highlight(RUST, TABBED_RUST, 0);
        let painted: Vec<String> = painter
            .paints
            .iter()
            .map(|paint| normalize_text(&TABBED_RUST[paint.range.clone()]))
            .collect();
        let shown: Vec<String> = Highlighter::for_token(RUST)
            .highlight_lines([TABBED_RUST])
            .concat()
            .into_iter()
            .map(|segment| segment.text)
            .filter(|text| !text.is_empty())
            .collect();
        assert_eq!(painted, shown);
    }

    #[test_case("x *"; "italic_openers")]
    #[test_case("x \\("; "unclosed_inline_math")]
    fn overlong_line_stays_plain_text(unit: &str) {
        let line = format!("{INLINE_CODE}{}", unit.repeat(MAX_LINE_BYTES / unit.len()));
        assert_eq!(styles(&line, 0), [(line.clone(), StyleToken::Text)]);
        let table = format!("|{line}|\n|-|");
        assert_eq!(
            styles(&table, 0),
            [
                ("|".to_owned(), StyleToken::TableBorder),
                (line, StyleToken::Text),
                ("|".to_owned(), StyleToken::TableBorder),
            ]
        );
    }

    #[test_case(MAX_LINE_BYTES => true; "at_cap")]
    #[test_case(MAX_LINE_BYTES + 1 => false; "past_cap")]
    fn inline_markup_is_picked_out_up_to_the_cap(len: usize) -> bool {
        let line = format!("{INLINE_CODE}{}", FILLER.repeat(len - INLINE_CODE.len()));
        styles(&line, 0)
            .iter()
            .any(|(_, style)| *style == StyleToken::InlineCode)
    }

    #[test]
    fn fence_with_an_overlong_line_stays_plain() {
        let overlong = FILLER.repeat(MAX_LINE_BYTES + 1);
        let text = format!("```{RUST}\n{TABBED_RUST}\n{overlong}\n```");
        assert_eq!(
            styles(&text, 1),
            [(TABBED_RUST.to_owned(), StyleToken::Text)]
        );
    }

    #[test_case("**b**", "b", Emphasis::BOLD; "bold")]
    #[test_case("*i*", "i", Emphasis::ITALIC; "italic")]
    #[test_case("~~s~~", "s", Emphasis::STRIKE; "strike")]
    #[test_case("***bi***", "bi", Emphasis::BOLD_ITALIC; "bold_italic")]
    #[test_case("# **h**", "h", Emphasis::BOLD; "bold_heading")]
    #[test_case("| h |\n| - |\n| c |", "h", Emphasis::BOLD; "table_header")]
    #[test_case("| h |\n| - |\n| c |", "c", Emphasis::default(); "table_body")]
    #[test_case("**b**", "**", Emphasis::default(); "delimiters_stay_plain")]
    fn content_carries_rendered_emphasis(text: &str, content: &str, emphasis: Emphasis) {
        assert_eq!(
            span_with_text(&source_lines(text), content).emphasis,
            emphasis
        );
    }

    #[test_case("[a](https://example.com/a)"; "explicit_link")]
    #[test_case("<https://example.com/a>"; "autolink")]
    #[test_case("see https://example.com/a"; "bare_url")]
    fn link_text_carries_its_target(text: &str) {
        let lines = source_lines(text);
        let linked: Vec<&Span> = lines[0]
            .spans
            .iter()
            .filter(|span| span.link.is_some())
            .collect();
        assert_eq!(linked.len(), 1, "{:?}", lines[0].spans);
        assert_eq!(linked[0].link.as_deref(), Some(LINK_TARGET));
        assert_ne!(linked[0].style, StyleToken::Syntax);
    }

    #[test]
    fn lines_take_the_kind_of_their_block() {
        let text = "# h\n- a\n```rust\nx\n```\n| a |\n| - |\n---\n$$\ny\n$$\n\ntext";
        let kinds: Vec<LineKind> = source_lines(text).into_iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Heading,
                LineKind::ListItem,
                LineKind::Code,
                LineKind::Code,
                LineKind::Code,
                LineKind::TableRow,
                LineKind::TableBorder,
                LineKind::HorizontalRule,
                LineKind::Math,
                LineKind::Math,
                LineKind::Math,
                LineKind::Blank,
                LineKind::Paragraph,
            ]
        );
    }
}
