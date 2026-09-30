//! The little Markdown the docs need understood: fenced code, ATX headings with explicit ids, and inline syntax
//! reduced to the text a reader sees.

use std::borrow::Cow;

use crate::Heading;
use crate::slug::slugify;

const FENCE_MIN_LEN: usize = 3;
const MAX_HEADING_LEVEL: usize = 6;
const EXPLICIT_ID_OPEN: &str = "{#";
const BREAK_TAG: &str = "br";
const URL_SCHEME_SEPARATOR: &str = "://";
const COMMENT_OPEN: &str = "<!--";
const COMMENT_CLOSE: &str = "-->";

/// Tracks fenced code blocks one line at a time.
#[derive(Default)]
pub(crate) struct Fences {
    open: Option<(u8, usize)>,
}

impl Fences {
    /// Feeds the next line and reports whether it is a fence line or code inside a fence.
    pub(crate) fn step(&mut self, line: &str) -> bool {
        match (self.open, fence_marker(line)) {
            (None, Some((marker, len, _))) => {
                self.open = Some((marker, len));
                true
            }
            (Some((open_marker, open_len)), Some((marker, len, rest)))
                if marker == open_marker && len >= open_len && rest.trim().is_empty() =>
            {
                self.open = None;
                true
            }
            (open, _) => open.is_some(),
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }
}

/// `text` without its HTML comment blocks, which the site never shows, such as the markers around a generated
/// region. A block opens on a line outside fenced code that starts with `<!--` and runs to the first line holding
/// `-->`.
pub(crate) fn without_comments(text: &str) -> Cow<'_, str> {
    if !text.contains(COMMENT_OPEN) {
        return Cow::Borrowed(text);
    }
    let mut kept = String::with_capacity(text.len());
    let mut fences = Fences::default();
    let mut in_comment = false;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let comment = if in_comment {
            Some(content)
        } else if fences.step(content) {
            None
        } else {
            content.trim_start().strip_prefix(COMMENT_OPEN)
        };
        match comment {
            Some(rest) => in_comment = !rest.contains(COMMENT_CLOSE),
            None => kept.push_str(line),
        }
    }
    Cow::Owned(kept)
}

fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let trimmed = line.trim_start();
    let marker = *trimmed.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = trimmed.bytes().take_while(|&byte| byte == marker).count();
    (len >= FENCE_MIN_LEN).then(|| (marker, len, &trimmed[len..]))
}

/// The level and text of an ATX heading line.
pub(crate) fn atx_heading(line: &str) -> Option<(u8, &str)> {
    let level = line.bytes().take_while(|&byte| byte == b'#').count();
    if level == 0 || level > MAX_HEADING_LEVEL {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some((u8::try_from(level).ok()?, rest.trim()))
}

/// Splits a trailing `{#id}` off heading text.
pub(crate) fn split_explicit_id(text: &str) -> (&str, Option<&str>) {
    let Some(inner) = text.strip_suffix('}') else {
        return (text, None);
    };
    match inner.rfind(EXPLICIT_ID_OPEN) {
        Some(open) => (
            text[..open].trim_end(),
            Some(&inner[open + EXPLICIT_ID_OPEN.len()..]),
        ),
        None => (text, None),
    }
}

/// Every heading outside fenced code, with Zola's ids: an explicit `{#id}` as written, otherwise the slug of the
/// visible text, and `-1`, `-2` for repeats.
pub(crate) fn headings(body: &str) -> Vec<Heading> {
    let mut fences = Fences::default();
    let mut taken: Vec<String> = Vec::new();
    let mut headings = Vec::new();
    for (line, text) in body.lines().enumerate() {
        if fences.step(text) {
            continue;
        }
        let Some((level, text)) = atx_heading(text) else {
            continue;
        };
        let (text, explicit) = split_explicit_id(text);
        let title = plain_inline(text);
        let anchor = match explicit {
            Some(id) => id.to_owned(),
            None => unique(slugify(&title), &taken),
        };
        taken.push(anchor.clone());
        headings.push(Heading {
            level,
            title,
            anchor,
            line,
        });
    }
    headings
}

fn unique(anchor: String, taken: &[String]) -> String {
    if !taken.contains(&anchor) {
        return anchor;
    }
    let mut suffix = 1;
    loop {
        let candidate = format!("{anchor}-{suffix}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Inline Markdown as a reader sees it: code without backticks, links as their label, no HTML tags or `**`.
pub(crate) fn plain_inline(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        match ch {
            '`' => {
                let (code, after) = code_span(rest);
                plain.push_str(code.trim_matches('`'));
                rest = after;
            }
            '!' if rest[1..].starts_with('[') => rest = &rest[1..],
            '[' => match link(rest) {
                Some((label, _, after)) => {
                    plain.push_str(&plain_inline(label));
                    rest = after;
                }
                None => {
                    plain.push('[');
                    rest = &rest[1..];
                }
            },
            '<' => match tag(rest) {
                Some((inner, after)) => {
                    if is_break(inner) {
                        plain.push(' ');
                    }
                    rest = after;
                }
                None => {
                    plain.push('<');
                    rest = &rest[1..];
                }
            },
            '*' if rest.starts_with("**") => rest = &rest[2..],
            '\\' if rest[1..].starts_with(|next: char| next.is_ascii_punctuation()) => {
                plain.push_str(&rest[1..2]);
                rest = &rest[2..];
            }
            _ => {
                plain.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    plain
}

/// Splits a code span, backticks included, off the front of `text`. An unclosed run of backticks is literal text.
pub(crate) fn code_span(text: &str) -> (&str, &str) {
    let run = text.bytes().take_while(|&byte| byte == b'`').count();
    let mut search = run;
    while let Some(found) = text[search..].find('`') {
        let start = search + found;
        let len = text[start..]
            .bytes()
            .take_while(|&byte| byte == b'`')
            .count();
        if len == run {
            return text.split_at(start + len);
        }
        search = start + len;
    }
    text.split_at(run)
}

/// Splits `[label](destination)` off the front of `text`: the label, the destination, and what follows.
pub(crate) fn link(text: &str) -> Option<(&str, &str, &str)> {
    let close = matching(text, b'[', b']')?;
    let after_label = &text[close + 1..];
    if !after_label.starts_with('(') {
        return None;
    }
    let end = matching(after_label, b'(', b')')?;
    Some((
        &text[1..close],
        &after_label[1..end],
        &after_label[end + 1..],
    ))
}

/// The index of the byte that closes the bracket `text` opens with.
fn matching(text: &str, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in text.bytes().enumerate() {
        if byte == open {
            depth += 1;
        } else if byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Splits an HTML tag off the front of `text`: what sits between `<` and `>`, and what follows. Autolinks such
/// as `<https://…>` are not tags.
pub(crate) fn tag(text: &str) -> Option<(&str, &str)> {
    let next = text[1..].chars().next()?;
    if !next.is_ascii_alphabetic() && next != '/' {
        return None;
    }
    let end = text.find('>')?;
    let inner = &text[1..end];
    (!inner.contains(URL_SCHEME_SEPARATOR)).then(|| (inner, &text[end + 1..]))
}

pub(crate) fn is_break(tag: &str) -> bool {
    tag.trim_end_matches('/')
        .trim()
        .eq_ignore_ascii_case(BREAK_TAG)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{Fences, headings, plain_inline, without_comments};

    #[test_case("`code_map` <span class=\"badge\">on demand</span>", "code_map on demand" ; "code and html")]
    #[test_case("See [Permissions](/docs/permissions/#modes).", "See Permissions." ; "link keeps its label")]
    #[test_case("**What changed:** a lot", "What changed: a lot" ; "strong emphasis")]
    #[test_case("one<br>two<br/>three", "one two three" ; "line breaks")]
    #[test_case("a < b and `x < y`", "a < b and x < y" ; "comparisons are not tags")]
    #[test_case("\\{{caudra.tools}}", "{{caudra.tools}}" ; "escaped punctuation")]
    #[test_case("``a ` b``", "a ` b" ; "double backtick code")]
    fn plain_inline_keeps_what_a_reader_sees(text: &str, expected: &str) {
        assert_eq!(plain_inline(text), expected);
    }

    #[test]
    fn fences_cover_code_and_their_own_lines() {
        let mut fences = Fences::default();
        let lines = [
            "text",
            "````markdown",
            "```bash",
            "## not a heading",
            "```",
            "````",
            "after",
        ];
        let inside: Vec<bool> = lines.iter().map(|line| fences.step(line)).collect();
        assert_eq!(inside, [false, true, true, true, true, true, false]);
    }

    #[test_case("a\n\n<!-- caudra-docgen:fields -->\n| x |\n<!-- /caudra-docgen:fields -->\n\nb\n", "a\n\n| x |\n\nb\n" ; "generated region markers")]
    #[test_case("a\n  <!--\n  # hidden\n  -->\nb", "a\nb" ; "a block over several lines")]
    #[test_case("```html\n<!-- shown -->\n```\n", "```html\n<!-- shown -->\n```\n" ; "a comment inside fenced code")]
    #[test_case("<!-- -->\n```\n<!-- still code -->\n```\n", "```\n<!-- still code -->\n```\n" ; "a fence after a comment")]
    fn html_comment_blocks_are_left_out(text: &str, expected: &str) {
        assert_eq!(without_comments(text), expected);
    }

    #[test]
    fn headings_take_explicit_ids_and_number_repeats() {
        let body = "# Title\n\n## Labels\n\n## Labels\n\n### `code_map` <span class=\"badge\">x</span> {#code_map}\n";
        let headings = headings(body);
        let anchors: Vec<(u8, &str, &str)> = headings
            .iter()
            .map(|heading| {
                (
                    heading.level,
                    heading.title.as_str(),
                    heading.anchor.as_str(),
                )
            })
            .collect();
        assert_eq!(
            anchors,
            [
                (1, "Title", "title"),
                (2, "Labels", "labels"),
                (2, "Labels", "labels-1"),
                (3, "code_map x", "code_map"),
            ]
        );
    }
}
