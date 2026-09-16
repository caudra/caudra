//! Markdown parser and width-aware renderer.
//!
//! The parser separates two orthogonal axes: `SpanKind` (text vs code) and
//! `Emphasis` (bold, italic, strike). They compose freely, so `***x***` is
//! bold+italic, and code inside bold keeps both.
//!
//! Every span and line carries the byte range it came from so consumers can
//! recover the original markdown on copy. Rendering drops syntax (heading
//! hashes, emphasis delimiters, fences, list markers); the ranges put it
//! back without storing a second copy of the text.

pub mod latex;
pub mod mermaid;
pub mod render;

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::{Not, Range};
use std::sync::Arc;

const BULLET: &str = "• ";
const LIST_INDENT_STEP: usize = 2;
const MAX_HEADING_LEVEL: u8 = 6;
const FENCE_MIN: usize = 3;
const MAX_LINK_DESTINATION_BYTES: usize = 2_048;
const MATH_FENCE: &str = "$$";
const MATH_PAREN_OPEN: &str = "\\(";
const MATH_PAREN_CLOSE: &str = "\\)";
const MATH_BRACKET_OPEN: &str = "\\[";
const MATH_BRACKET_CLOSE: &str = "\\]";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Emphasis {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
}

impl Emphasis {
    pub const BOLD: Self = Self {
        bold: true,
        italic: false,
        strike: false,
        underline: false,
    };
    pub const ITALIC: Self = Self {
        bold: false,
        italic: true,
        strike: false,
        underline: false,
    };
    pub const BOLD_ITALIC: Self = Self {
        bold: true,
        italic: true,
        strike: false,
        underline: false,
    };
    pub const STRIKE: Self = Self {
        bold: false,
        italic: false,
        strike: true,
        underline: false,
    };
    pub const UNDERLINE: Self = Self {
        bold: false,
        italic: false,
        strike: false,
        underline: true,
    };

    pub fn merge(self, other: Self) -> Self {
        Self {
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
            strike: self.strike || other.strike,
            underline: self.underline || other.underline,
        }
    }

    pub fn is_empty(self) -> bool {
        !self.bold && !self.italic && !self.strike && !self.underline
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanKind {
    Text,
    Code,
    /// LaTeX source. The renderer decides how to present it, so `text` here
    /// is the math itself with its delimiters stripped.
    Math,
}

/// Where a rendered span came from in the parsed text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Source {
    pub range: Range<u32>,
    /// `false` when the rendered text is not a byte-identical slice of
    /// `range`, which makes the span atomic: any selection touching it
    /// yields the whole range. Math and links set this; emphasis and code
    /// do not, because their content is still a verbatim slice and only the
    /// delimiters live outside it.
    pub verbatim: bool,
}

impl Source {
    pub fn verbatim(range: Range<u32>) -> Self {
        Self {
            range,
            verbatim: true,
        }
    }

    pub fn atomic(range: Range<u32>) -> Self {
        Self {
            range,
            verbatim: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineSpan {
    pub text: String,
    pub kind: SpanKind,
    pub emphasis: Emphasis,
    pub source: Source,
    pub link: Option<Arc<str>>,
}

impl InlineSpan {
    pub fn text(text: impl Into<String>, emphasis: Emphasis, source: Source) -> Self {
        Self {
            text: text.into(),
            kind: SpanKind::Text,
            emphasis,
            source,
            link: None,
        }
    }

    pub fn code(text: impl Into<String>, emphasis: Emphasis, source: Source) -> Self {
        Self {
            text: text.into(),
            kind: SpanKind::Code,
            emphasis,
            source,
            link: None,
        }
    }

    pub fn math(text: impl Into<String>, emphasis: Emphasis, source: Source) -> Self {
        Self {
            text: text.into(),
            kind: SpanKind::Math,
            emphasis,
            source,
            link: None,
        }
    }

    fn with_link(mut self, link: Option<Arc<str>>) -> Self {
        self.link = link;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockKind {
    Paragraph,
    Heading(u8),
    UnorderedListItem { depth: usize },
    OrderedListItem { depth: usize, marker: String },
    HorizontalRule,
}

/// Inline delimiters are kept intact here. Emphasis and code parsing is
/// deferred to `parse_inline` so callers can wrap before deciding styles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineBlock {
    pub kind: BlockKind,
    pub inline: String,
    /// The whole source line, including the heading hashes or list marker
    /// that `inline` excludes.
    pub source: Range<u32>,
    /// Absolute offset where `inline` starts, so inline spans can report
    /// positions in the original text.
    pub inline_start: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Block {
    Lines(Vec<LineBlock>),
    Code {
        lang: String,
        code: String,
        /// The fenced block including both fence lines.
        source: Range<u32>,
        /// Absolute offset of the first byte of `code`.
        code_start: u32,
        /// Whether the closing fence has arrived. Streaming shows a block
        /// before it does, and anything that reshapes the content whole
        /// (a diagram) has to wait for the real end.
        closed: bool,
    },
    Table {
        rows: Vec<Vec<String>>,
        header_end: usize,
        /// Source line per entry of `rows`.
        row_sources: Vec<Range<u32>>,
        /// The `| --- |` line, which `rows` drops.
        separator: Range<u32>,
    },
    /// Display math. Parsed as one unit so the line classifier never sees
    /// its contents: a `- x` row inside an equation is not a bullet, and
    /// `---` is not a horizontal rule.
    Math {
        latex: String,
        /// The block including both delimiters.
        source: Range<u32>,
    },
}

pub fn parse(text: &str) -> Vec<Block> {
    parse_at(text, 0)
}

/// `base` is the absolute offset of `text` in the document it was sliced
/// from, so callers that trim before parsing still get ranges that index
/// the untrimmed string.
pub fn parse_at(text: &str, base: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut rest = text;
    let mut base = base;
    while let Some(found) = find_fenced_block(rest) {
        let before = rest[..found.before_end()].trim_end_matches('\n');
        if !before.is_empty() {
            blocks.extend(split_normal_blocks(before, base));
        }
        let block_end = match found {
            Fenced::Code(fence) => {
                blocks.push(Block::Code {
                    lang: fence.lang.to_owned(),
                    code: fence.code.to_owned(),
                    source: range_at(base + fence.before_end, base + fence.block_end),
                    code_start: (base + fence.code_start) as u32,
                    closed: fence.closed,
                });
                fence.block_end
            }
            Fenced::Math(fence) => {
                blocks.push(Block::Math {
                    latex: fence.latex.to_owned(),
                    source: range_at(base + fence.before_end, base + fence.block_end),
                });
                fence.block_end
            }
        };
        let skip =
            block_end + rest[block_end..].len() - rest[block_end..].trim_start_matches('\n').len();
        rest = &rest[skip..];
        base += skip;
    }
    if !rest.is_empty() {
        blocks.extend(split_normal_blocks(rest, base));
    }
    blocks
}

enum Fenced<'a> {
    Code(CodeFence<'a>),
    Math(MathFence<'a>),
}

impl Fenced<'_> {
    fn before_end(&self) -> usize {
        match self {
            Self::Code(f) => f.before_end,
            Self::Math(f) => f.before_end,
        }
    }
}

/// Whichever of a code fence or a display-math block opens first. Order
/// matters: `$$` inside a code block is not math, and ``` inside display
/// math is not code.
fn find_fenced_block(text: &str) -> Option<Fenced<'_>> {
    match (find_code_fence(text), find_math_fence(text)) {
        (Some(code), Some(math)) if math.before_end < code.before_end => Some(Fenced::Math(math)),
        (Some(code), _) => Some(Fenced::Code(code)),
        (None, Some(math)) => Some(Fenced::Math(math)),
        (None, None) => None,
    }
}

struct MathFence<'a> {
    before_end: usize,
    latex: &'a str,
    block_end: usize,
}

fn find_math_fence(text: &str) -> Option<MathFence<'_>> {
    let mut offset = 0;
    let mut lines = text.split('\n');
    while let Some(line) = lines.next() {
        let line_start = offset;
        offset += line.len() + 1;
        let trimmed = line.trim();
        let Some((open, close)) = math_block_opener(trimmed) else {
            continue;
        };

        // `$$ E = mc^2 $$` all on one line.
        let rest = &trimmed[open.len()..];
        if let Some(inner) = rest.strip_suffix(close)
            && !inner.trim().is_empty()
        {
            return Some(MathFence {
                before_end: line_start,
                latex: inner,
                block_end: line_start + line.len(),
            });
        }
        if !rest.trim().is_empty() {
            continue;
        }

        let content_start = offset;
        for next in lines.by_ref() {
            let next_start = offset;
            offset += next.len() + 1;
            if next.trim() == close {
                return Some(MathFence {
                    before_end: line_start,
                    latex: &text[content_start..next_start.saturating_sub(1).max(content_start)],
                    block_end: next_start + next.len(),
                });
            }
        }
        // Unterminated: take the rest, which keeps a streaming equation from
        // being re-parsed as headings and bullets on every chunk.
        return Some(MathFence {
            before_end: line_start,
            latex: &text[content_start.min(text.len())..],
            block_end: text.len(),
        });
    }
    None
}

fn math_block_opener(trimmed: &str) -> Option<(&'static str, &'static str)> {
    if trimmed.starts_with(MATH_FENCE) {
        return Some((MATH_FENCE, MATH_FENCE));
    }
    if trimmed.starts_with(MATH_BRACKET_OPEN) {
        return Some((MATH_BRACKET_OPEN, MATH_BRACKET_CLOSE));
    }
    None
}

fn range_at(start: usize, end: usize) -> Range<u32> {
    start as u32..end as u32
}

fn split_normal_blocks(text: &str, base: usize) -> Vec<Block> {
    let mut lines_with_offsets: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        lines_with_offsets.push((offset, line));
        offset += line.len() + 1;
    }

    let mut blocks: Vec<Block> = Vec::new();
    let mut normal_start: Option<usize> = None;
    let mut i = 0;

    while i < lines_with_offsets.len() {
        let (_, line) = lines_with_offsets[i];
        if is_table_row(line) {
            let table_start = i;
            let header_cols = parse_table_cells(line).len();
            let mut sep_idx = None;
            let mut j = i;
            while j < lines_with_offsets.len() && is_table_row(lines_with_offsets[j].1) {
                if sep_idx.is_none()
                    && is_separator_row(lines_with_offsets[j].1)
                    && parse_table_cells(lines_with_offsets[j].1).len() >= header_cols
                {
                    sep_idx = Some(j - table_start);
                }
                j += 1;
            }
            if let Some(si) = sep_idx
                && j - table_start >= 2
            {
                if let Some(ns) = normal_start.take() {
                    let start = lines_with_offsets[ns].0;
                    let end = lines_with_offsets[table_start].0;
                    let raw = &text[start..end];
                    let lead = raw.len() - raw.trim_start_matches('\n').len();
                    let slice = raw.trim_matches('\n');
                    if !slice.is_empty() {
                        blocks.push(Block::Lines(lines_to_blocks(slice, base + start + lead)));
                    }
                }

                let table_end = if j < lines_with_offsets.len()
                    && j == lines_with_offsets.len() - 1
                    && lines_with_offsets[j].1.trim_start().starts_with('|')
                {
                    j + 1
                } else {
                    j
                };

                let mut rows = Vec::new();
                let mut row_sources = Vec::new();
                let mut separator = 0..0;
                for (k, &(off, line)) in lines_with_offsets[table_start..table_end]
                    .iter()
                    .enumerate()
                {
                    let source = range_at(base + off, base + off + line.len());
                    if k == si {
                        separator = source;
                    } else {
                        rows.push(parse_table_cells(line));
                        row_sources.push(source);
                    }
                }
                blocks.push(Block::Table {
                    rows,
                    header_end: si,
                    row_sources,
                    separator,
                });
                i = table_end;
                continue;
            }
        }

        if normal_start.is_none() {
            normal_start = Some(i);
        }
        i += 1;
    }

    if let Some(ns) = normal_start {
        let start = lines_with_offsets[ns].0;
        let raw = &text[start..];
        let lead = raw.len() - raw.trim_start_matches('\n').len();
        let content = raw.trim_start_matches('\n');
        if !content.is_empty() {
            blocks.push(Block::Lines(lines_to_blocks(content, base + start + lead)));
        }
    }

    if blocks.is_empty() {
        blocks.push(Block::Lines(lines_to_blocks(text, base)));
    }

    blocks
}

fn lines_to_blocks(text: &str, base: usize) -> Vec<LineBlock> {
    let mut offset = base;
    text.split('\n')
        .map(|line| {
            let block = classify_line(line, offset);
            offset += line.len() + 1;
            block
        })
        .collect()
}

/// `start` is the absolute offset of `line`. `inline_start` points past the
/// marker or hashes so inline spans land in the original text.
fn classify_line(line: &str, start: usize) -> LineBlock {
    let source = range_at(start, start + line.len());
    // Only valid for suffixes of `line`; headings trim their end and so
    // report their own offset instead.
    let at = |rest: &str| (start + line.len() - rest.len()) as u32;

    if is_horizontal_rule(line) {
        return LineBlock {
            kind: BlockKind::HorizontalRule,
            inline: String::new(),
            source,
            inline_start: start as u32,
        };
    }
    if let Some((level, content, content_start)) = parse_heading(line) {
        return LineBlock {
            kind: BlockKind::Heading(level),
            inline: content.to_owned(),
            source,
            inline_start: (start + content_start) as u32,
        };
    }
    if let Some((indent_spaces, rest)) = parse_unordered_marker(line) {
        return LineBlock {
            kind: BlockKind::UnorderedListItem {
                depth: indent_spaces / LIST_INDENT_STEP,
            },
            inline: rest.to_owned(),
            source,
            inline_start: at(rest),
        };
    }
    if let Some((indent_spaces, marker, rest)) = parse_ordered_marker(line) {
        return LineBlock {
            kind: BlockKind::OrderedListItem {
                depth: indent_spaces / LIST_INDENT_STEP,
                marker: marker.to_owned(),
            },
            inline: rest.to_owned(),
            source,
            inline_start: at(rest),
        };
    }
    LineBlock {
        kind: BlockKind::Paragraph,
        inline: line.to_owned(),
        source,
        inline_start: start as u32,
    }
}

pub fn block_prefix(kind: &BlockKind) -> Option<String> {
    match kind {
        BlockKind::UnorderedListItem { depth } => {
            Some(format!("{}{BULLET}", " ".repeat(depth * LIST_INDENT_STEP)))
        }
        BlockKind::OrderedListItem { depth, marker } => {
            Some(format!("{}{marker} ", " ".repeat(depth * LIST_INDENT_STEP)))
        }
        BlockKind::Paragraph | BlockKind::Heading(_) | BlockKind::HorizontalRule => None,
    }
}

/// Returns the level, the trimmed content, and the content's byte offset in
/// `line`. The offset cannot be recovered from the content because trimming
/// makes it a non-suffix slice.
fn parse_heading(line: &str) -> Option<(u8, &str, usize)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > MAX_HEADING_LEVEL as usize {
        return None;
    }
    let rest = &line[hashes..];
    let level = hashes as u8;
    if let Some(stripped) = rest.strip_prefix(' ') {
        Some((level, stripped.trim_end(), hashes + 1))
    } else if rest.is_empty() {
        Some((level, "", hashes))
    } else {
        None
    }
}

fn parse_unordered_marker(line: &str) -> Option<(usize, &str)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    let rest = &line[indent..];
    let marker = rest.as_bytes().first()?;
    if !matches!(marker, b'-' | b'*' | b'+') {
        return None;
    }
    let after = &rest[1..];
    let stripped = after.strip_prefix(' ')?;
    Some((indent, stripped))
}

fn parse_ordered_marker(line: &str) -> Option<(usize, &str, &str)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    let rest = &line[indent..];
    let digits_end = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits_end == 0 {
        return None;
    }
    let after_digits = &rest[digits_end..];
    if !after_digits.starts_with(". ") {
        return None;
    }
    Some((indent, &rest[..=digits_end], &after_digits[2..]))
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let first = match trimmed.as_bytes().first() {
        Some(b'-' | b'*' | b'_') => trimmed.as_bytes()[0],
        _ => return false,
    };
    trimmed.bytes().all(|b| b == first || b == b' ')
        && trimmed.bytes().filter(|&b| b == first).count() >= 3
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.matches('|').count() >= 2
}

fn is_separator_row(line: &str) -> bool {
    if !is_table_row(line) {
        return false;
    }
    parse_table_cells(line)
        .iter()
        .all(|cell| cell.bytes().all(|b| matches!(b, b'-' | b':')) && cell.contains('-'))
}

fn parse_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);

    let bytes = inner.as_bytes();
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run_len = count_backtick_run(bytes, i);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, i, run_len) {
                current.push_str(&inner[i..close_end]);
                i = close_end;
            } else {
                current.push_str(&inner[i..]);
                i = bytes.len();
            }
        } else if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            current.push('|');
            i += 2;
        } else if bytes[i] == b'|' {
            cells.push(current.trim().to_owned());
            current = String::new();
            i += 1;
        } else {
            let ch = inner[i..].chars().next().unwrap();
            current.push(ch);
            i += ch.len_utf8();
        }
    }

    cells.push(current.trim().to_owned());
    cells
}

struct CodeFence<'a> {
    before_end: usize,
    lang: &'a str,
    code: &'a str,
    code_start: usize,
    block_end: usize,
    closed: bool,
}

fn find_code_fence(text: &str) -> Option<CodeFence<'_>> {
    let bytes = text.as_bytes();
    let mut search_from = 0;
    while search_from < bytes.len() {
        let pos = text[search_from..].find("```")?;
        let abs = search_from + pos;
        if abs != 0 && bytes[abs - 1] != b'\n' {
            search_from = abs + FENCE_MIN;
            continue;
        }
        let fence_len = FENCE_MIN
            + bytes[abs + FENCE_MIN..]
                .iter()
                .take_while(|&&b| b == b'`')
                .count();
        let after_ticks = abs + fence_len;
        let Some(nl) = text[after_ticks..].find('\n') else {
            search_from = abs + fence_len;
            continue;
        };
        let info = &text[after_ticks..after_ticks + nl];
        if info.contains('`') {
            search_from = abs + fence_len;
            continue;
        }
        let lang = info.trim();
        let code_start = after_ticks + nl + 1;
        let fence_str = "`".repeat(fence_len);
        let mut offset = 0;
        let mut close: Option<(usize, usize)> = None;
        for line in text[code_start..].split('\n') {
            let trimmed = line.trim_end();
            if trimmed.len() >= fence_len
                && trimmed.starts_with(&fence_str)
                && !trimmed[fence_len..].starts_with('`')
            {
                close = Some((offset, line.len()));
                break;
            }
            offset += line.len() + 1;
        }
        let (code, block_end) = if let Some((close_off, close_line_len)) = close {
            let raw_end = code_start + close_off;
            let code_end = if raw_end > code_start && bytes[raw_end - 1] == b'\n' {
                raw_end - 1
            } else {
                raw_end
            };
            let trailing_start = code_start + close_off + fence_len;
            let trailing_end = code_start + close_off + close_line_len;
            let block_end = if text[trailing_start..trailing_end].trim().is_empty() {
                trailing_end
            } else {
                trailing_start
            };
            (&text[code_start..code_end], block_end)
        } else {
            (&text[code_start..], text.len())
        };
        return Some(CodeFence {
            before_end: abs,
            lang,
            code,
            code_start,
            block_end,
            closed: close.is_some(),
        });
    }
    None
}

/// Emphasis composes additively. Code spans are atomic and carry the
/// surrounding emphasis as a separate modifier.
pub fn parse_inline(text: &str) -> Vec<InlineSpan> {
    parse_inline_at(text, 0)
}

/// `offset` is the absolute position of `text` in the document. Every span
/// reports its own slice of it, so consumers can map a rendered cell back to
/// the markdown that produced it.
pub fn parse_inline_at(text: &str, offset: u32) -> Vec<InlineSpan> {
    parse_inline_impl(text, offset, Emphasis::default(), ParseMode::WithCode, true)
}

/// Rewrites a still-streaming markdown prefix so its open tail renders the
/// way it will once the rest arrives.
///
/// A delimiter resolves only when its closer is in the text, so a growing
/// `**bold` draws its asterisks and then loses them the moment the run
/// closes: two columns vanish and the paragraph rewraps under the reader.
/// This closes what the tail leaves open, and drops a trailing fragment too
/// short to classify, so content is styled as it arrives and never moves
/// afterwards.
///
/// Complete markdown comes back borrowed and untouched, so this is a no-op
/// on anything already settled. Only the outermost open delimiter is closed:
/// a nested one stays literal for the few frames until its own closer lands.
pub fn close_open_tail(text: &str) -> Cow<'_, str> {
    let Some(inline_start) = tail_inline_start(text) else {
        return Cow::Borrowed(text);
    };
    let mut end = text.len();
    loop {
        match scan_open_tail(&text[inline_start..end]) {
            OpenTail::Settled => break,
            OpenTail::Close(closer) => return Cow::Owned(format!("{}{closer}", &text[..end])),
            OpenTail::Truncate(at) => {
                let cut = inline_start + at;
                if cut >= end {
                    break;
                }
                end = cut;
            }
        }
    }
    Cow::Borrowed(&text[..end])
}

/// Where the last line's inline content starts, or `None` when the tail is
/// inside a fenced block: those already stream whole, and a horizontal rule
/// has no inline content to close.
fn tail_inline_start(text: &str) -> Option<usize> {
    let mut rest = text;
    let mut base = 0;
    while let Some(found) = find_fenced_block(rest) {
        let block_end = match &found {
            Fenced::Code(fence) => fence.block_end,
            Fenced::Math(fence) => fence.block_end,
        };
        if block_end >= rest.len() {
            return None;
        }
        rest = &rest[block_end..];
        base += block_end;
    }
    let line_start = base + rest.rfind('\n').map_or(0, |nl| nl + 1);
    let line = classify_line(&text[line_start..], line_start);
    matches!(line.kind, BlockKind::HorizontalRule)
        .not()
        .then_some(line.inline_start as usize)
}

enum OpenTail {
    Settled,
    /// Hold back from this offset: the fragment there cannot be classified
    /// until more of it arrives.
    Truncate(usize),
    /// Append this to resolve what the tail left open.
    Close(Cow<'static, str>),
}

/// Walks the tail the way [`parse_inline_impl`] does, skipping whatever
/// already resolved, and reports the first construct still open.
fn scan_open_tail(text: &str) -> OpenTail {
    let bytes = text.as_bytes();
    let label_ends = bytes.contains(&b'[').then(|| scan_link_label_ends(text));
    let mut pos = 0;

    while pos < bytes.len() {
        if let Some(math) = try_inline_math(text, pos) {
            pos = math.end;
            continue;
        }

        if bytes[pos] == b'[' || (bytes[pos] == b'!' && bytes.get(pos + 1) == Some(&b'[')) {
            let open = pos + usize::from(bytes[pos] == b'!');
            let resolved = label_ends
                .as_ref()
                .and_then(|ends| ends.get(&open))
                .copied()
                .and_then(|label_end| try_explicit_link(text, open, label_end));
            match resolved {
                Some(link) => pos = link.end,
                None => return open_link(text, pos, open, label_ends.as_ref()),
            }
            continue;
        }

        if let Some(link) = try_autolink(text, pos) {
            pos = link.end;
            continue;
        }

        if bytes[pos] == b'<' && starts_http_scheme_at(text, pos + 1) {
            return OpenTail::Close(Cow::Borrowed(">"));
        }

        if let Some(end) = bare_url_end(text, pos) {
            pos = end;
            continue;
        }

        // A delimiter run reaching the end cannot be classified yet: the
        // parser needs the byte after it to tell an opener from literal text,
        // and a single `~` is not even a candidate until its pair arrives.
        if matches!(bytes[pos], b'*' | b'_' | b'~' | b'`')
            && pos + count_run(bytes, pos, bytes[pos]) >= bytes.len()
        {
            return OpenTail::Truncate(pos);
        }

        if bytes[pos] == b'`' {
            let run = count_backtick_run(bytes, pos);
            if let Some((cs, ce, close_end)) = find_code_span_close(bytes, pos, run)
                && ce > cs
            {
                pos = close_end;
                continue;
            }
            return hold_partial_closer(bytes, pos, Cow::Owned("`".repeat(run)));
        }

        let outcome = match bytes[pos] {
            b'*' => try_star_emphasis(bytes, pos),
            b'~' => try_strike_emphasis(bytes, pos),
            b'_' => try_underscore_emphasis(bytes, pos),
            _ => InlineMatch::None,
        };
        match outcome {
            InlineMatch::Found {
                close, delim_len, ..
            } => {
                // A run that resolved against fewer characters than it opened
                // with, right at the end, is its own closer still arriving:
                // `***both**` is not bold `*both`, it is bold-italic `both`
                // one star short.
                if close + delim_len >= bytes.len() && count_run(bytes, pos, bytes[pos]) > delim_len
                {
                    return OpenTail::Truncate(close);
                }
                pos = close + delim_len;
            }
            InlineMatch::Skip(n) => match open_emphasis(bytes, pos) {
                Some(delim) => return hold_partial_closer(bytes, pos, Cow::Borrowed(delim)),
                None => pos += n,
            },
            InlineMatch::None => pos += 1,
        }
    }
    OpenTail::Settled
}

/// A short run of the opener's own character at the very end is its closer
/// arriving one byte at a time. Held back rather than treated as content, so
/// `**bold*` does not get a third asterisk appended and draw the stray.
fn hold_partial_closer(bytes: &[u8], pos: usize, delim: Cow<'static, str>) -> OpenTail {
    let ch = delim.as_bytes()[0];
    let content_start = pos + delim.len();
    let mut start = bytes.len();
    while start > content_start && bytes[start - 1] == ch {
        start -= 1;
    }
    match start < bytes.len() && bytes.len() - start < delim.len() {
        true => OpenTail::Truncate(start),
        false => OpenTail::Close(delim),
    }
}

/// What would close the delimiter run at `pos`, for a run the parser could
/// not resolve. `None` leaves it as the literal text it already is.
fn open_emphasis(bytes: &[u8], pos: usize) -> Option<&'static str> {
    let ch = bytes[pos];
    let run = count_run(bytes, pos, ch);
    let delim = match (ch, run) {
        (b'*', 3..) => "***",
        (b'*', 2) => "**",
        (b'~', 2) => "~~",
        (b'*', 1) => "*",
        (b'_', 1) => "_",
        _ => return None,
    };
    let opens = match run {
        1 => is_valid_italic_open(bytes, pos),
        _ => opens_run(bytes, pos + run),
    };
    opens.then_some(delim)
}

/// A link the tail has not finished. Its label is held back until the
/// destination starts, and drawn from then on: closing the parenthesis lets
/// the label settle into place once instead of appearing as raw syntax and
/// collapsing when the real `)` lands.
fn open_link(
    text: &str,
    start: usize,
    open: usize,
    label_ends: Option<&HashMap<usize, usize>>,
) -> OpenTail {
    let held = OpenTail::Truncate(start);
    let Some(label_end) = label_ends.and_then(|ends| ends.get(&open)).copied() else {
        return held;
    };
    // Cheaper to ask the real parser whether one parenthesis is all that is
    // missing than to re-derive its destination and title rules here.
    let closed = format!("{text})");
    match try_explicit_link(&closed, open, label_end) {
        Some(link) if link.end == closed.len() => OpenTail::Close(Cow::Borrowed(")")),
        _ => held,
    }
}

/// `EmphasisOnly` is for rescanning a region the outer pass already split on
/// code, so we don't re-recognize backticks we've already consumed.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ParseMode {
    WithCode,
    EmphasisOnly,
}

fn parse_inline_impl(
    text: &str,
    offset: u32,
    emphasis: Emphasis,
    mode: ParseMode,
    allow_links: bool,
) -> Vec<InlineSpan> {
    let bytes = text.as_bytes();
    let link_label_ends = (allow_links && mode == ParseMode::WithCode && bytes.contains(&b'['))
        .then(|| scan_link_label_ends(text));
    let mut spans = Vec::new();
    let mut pos = 0;
    let mut plain_start = 0;

    // Emphasis and code delimiters are dropped from the span text but stay
    // inside the enclosing line's range, so spans stay verbatim slices.
    let flush_plain = |spans: &mut Vec<InlineSpan>, plain: &str, at: usize| {
        if plain.is_empty() {
            return;
        }
        let at = offset + at as u32;
        match mode {
            ParseMode::WithCode => spans.extend(parse_inline_impl(
                plain,
                at,
                emphasis,
                ParseMode::EmphasisOnly,
                allow_links,
            )),
            ParseMode::EmphasisOnly => spans.push(InlineSpan::text(
                plain.to_owned(),
                emphasis,
                Source::verbatim(at..at + plain.len() as u32),
            )),
        }
    };

    while pos < bytes.len() {
        // Math is atomic like code, so it is recognised in the outer pass.
        // That also means emphasis never runs inside it and `_`/`*` in an
        // equation survive untouched.
        if mode == ParseMode::WithCode
            && let Some(math) = try_inline_math(text, pos)
        {
            flush_plain(&mut spans, &text[plain_start..pos], plain_start);
            spans.push(InlineSpan::math(
                text[math.content].to_owned(),
                emphasis,
                Source::atomic(offset + pos as u32..offset + math.end as u32),
            ));
            pos = math.end;
            plain_start = pos;
            continue;
        }

        if allow_links
            && mode == ParseMode::WithCode
            && bytes[pos] == b'!'
            && let Some(label_end) = link_label_ends
                .as_ref()
                .and_then(|ends| ends.get(&(pos + 1)))
                .copied()
            && let Some(image) = try_explicit_link(text, pos + 1, label_end)
        {
            pos = image.end;
            continue;
        }

        if allow_links
            && mode == ParseMode::WithCode
            && !pos.checked_sub(1).is_some_and(|i| bytes[i] == b'!')
            && let Some(label_end) = link_label_ends
                .as_ref()
                .and_then(|ends| ends.get(&pos))
                .copied()
            && let Some(link) = try_explicit_link(text, pos, label_end)
        {
            flush_plain(&mut spans, &text[plain_start..pos], plain_start);
            let target = http_target(&text[link.target.clone()]);
            let source = Source::atomic(offset + pos as u32..offset + link.end as u32);
            let mut label = parse_inline_impl(
                &text[link.label.clone()],
                offset + link.label.start as u32,
                emphasis,
                ParseMode::WithCode,
                false,
            );
            for span in &mut label {
                span.source = source.clone();
                span.link = target.clone();
            }
            spans.extend(label);
            pos = link.end;
            plain_start = pos;
            continue;
        }

        if allow_links
            && mode == ParseMode::WithCode
            && let Some(link) = try_autolink(text, pos)
        {
            flush_plain(&mut spans, &text[plain_start..pos], plain_start);
            let target = Arc::<str>::from(&text[link.target.clone()]);
            spans.push(
                InlineSpan::text(
                    target.to_string(),
                    emphasis,
                    Source::atomic(offset + pos as u32..offset + link.end as u32),
                )
                .with_link(Some(target)),
            );
            pos = link.end;
            plain_start = pos;
            continue;
        }

        if allow_links
            && mode == ParseMode::WithCode
            && let Some(end) = bare_url_end(text, pos)
        {
            flush_plain(&mut spans, &text[plain_start..pos], plain_start);
            let target = Arc::<str>::from(&text[pos..end]);
            spans.push(
                InlineSpan::text(
                    target.to_string(),
                    emphasis,
                    Source::verbatim(offset + pos as u32..offset + end as u32),
                )
                .with_link(Some(target)),
            );
            pos = end;
            plain_start = pos;
            continue;
        }

        if mode == ParseMode::WithCode && bytes[pos] == b'`' {
            let run_len = count_backtick_run(bytes, pos);
            if let Some((cs, ce, close_end)) = find_code_span_close(bytes, pos, run_len)
                && ce > cs
            {
                flush_plain(&mut spans, &text[plain_start..pos], plain_start);
                spans.push(InlineSpan::code(
                    text[cs..ce].to_owned(),
                    emphasis,
                    Source::verbatim(offset + cs as u32..offset + ce as u32),
                ));
                pos = close_end;
                plain_start = pos;
                continue;
            }
            pos += run_len;
            continue;
        }

        let outcome = match bytes[pos] {
            b'*' => try_star_emphasis(bytes, pos),
            b'~' => try_strike_emphasis(bytes, pos),
            b'_' => try_underscore_emphasis(bytes, pos),
            _ => InlineMatch::None,
        };

        match outcome {
            InlineMatch::Found {
                emphasis: found,
                content_start,
                close,
                delim_len,
            } => {
                flush_plain(&mut spans, &text[plain_start..pos], plain_start);
                spans.extend(parse_inline_impl(
                    &text[content_start..close],
                    offset + content_start as u32,
                    emphasis.merge(found),
                    mode,
                    allow_links,
                ));
                pos = close + delim_len;
                plain_start = pos;
            }
            InlineMatch::Skip(n) => pos += n,
            InlineMatch::None => pos += 1,
        }
    }

    if plain_start < bytes.len() {
        flush_plain(&mut spans, &text[plain_start..], plain_start);
    }
    spans
}

struct ExplicitLink {
    label: Range<usize>,
    target: Range<usize>,
    end: usize,
}

fn try_explicit_link(text: &str, pos: usize, label_end: usize) -> Option<ExplicitLink> {
    let bytes = text.as_bytes();
    let mut limit = label_end
        .saturating_add(2)
        .saturating_add(MAX_LINK_DESTINATION_BYTES)
        .min(bytes.len());
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    if bytes.get(pos) != Some(&b'[') {
        return None;
    }

    if bytes.get(label_end + 1) != Some(&b'(') {
        return None;
    }
    let mut at = label_end + 2;
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }

    let (target, mut at) = if bytes.get(at) == Some(&b'<') {
        let start = at + 1;
        let end = find_unescaped(&bytes[..limit], start, b'>')?;
        if text[start..end].contains(['\n', '\r', '<']) {
            return None;
        }
        (start..end, end + 1)
    } else {
        let start = at;
        let mut depth = 0usize;
        while at < limit {
            match bytes[at] {
                b'\\' if at + 1 < bytes.len() => at += 2,
                b'(' => {
                    depth += 1;
                    at += 1;
                }
                b')' if depth > 0 => {
                    depth -= 1;
                    at += 1;
                }
                b')' | b' ' | b'\t' | b'\n' | b'\r' if depth == 0 => break,
                _ => at += 1,
            }
        }
        if at == start {
            return None;
        }
        (start..at, at)
    };

    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    if bytes.get(at) != Some(&b')') {
        let quote = *bytes.get(at)?;
        let close = match quote {
            b'\'' | b'"' => quote,
            b'(' => b')',
            _ => return None,
        };
        at = find_unescaped(&bytes[..limit], at + 1, close)? + 1;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if bytes.get(at) != Some(&b')') {
            return None;
        }
    }

    Some(ExplicitLink {
        label: pos + 1..label_end,
        target,
        end: at + 1,
    })
}

fn scan_link_label_ends(text: &str) -> HashMap<usize, usize> {
    let bytes = text.as_bytes();
    let mut ends = HashMap::new();
    let mut openings = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if matches!(bytes[at], b'$' | b'\\')
            && let Some(math) = try_inline_math(text, at)
        {
            at = math.end;
            continue;
        }
        match bytes[at] {
            b'\\' if at + 1 < bytes.len() => at += 2,
            b'`' => {
                let run_len = count_backtick_run(bytes, at);
                at = find_code_span_close(bytes, at, run_len)
                    .map_or(at + run_len, |(_, _, close_end)| close_end);
            }
            b'[' => {
                openings.push(at);
                at += 1;
            }
            b']' => {
                if let Some(open) = openings.pop() {
                    ends.insert(open, at);
                }
                at += 1;
            }
            _ => at += 1,
        }
    }
    ends
}

fn find_unescaped(bytes: &[u8], mut at: usize, needle: u8) -> Option<usize> {
    while at < bytes.len() {
        if bytes[at] == b'\\' {
            at += 2;
        } else if bytes[at] == needle {
            return Some(at);
        } else {
            at += 1;
        }
    }
    None
}

struct Autolink {
    target: Range<usize>,
    end: usize,
}

fn try_autolink(text: &str, pos: usize) -> Option<Autolink> {
    if text.as_bytes().get(pos) != Some(&b'<') {
        return None;
    }
    let start = pos + 1;
    if !starts_http_scheme(&text[start..]) {
        return None;
    }
    let search_end = start
        .saturating_add(MAX_LINK_DESTINATION_BYTES + 1)
        .min(text.len());
    let close = text.as_bytes()[start..search_end]
        .iter()
        .position(|byte| *byte == b'>')?
        + start;
    if text[start..close]
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '<')
    {
        return None;
    }
    Some(Autolink {
        target: start..close,
        end: close + 1,
    })
}

fn http_target(target: &str) -> Option<Arc<str>> {
    let mut decoded = String::with_capacity(target.len());
    let mut chars = target.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(escaped) = chars.clone().next()
            && escaped.is_ascii_punctuation()
        {
            decoded.push(escaped);
            chars.next();
        } else {
            decoded.push(ch);
        }
    }
    starts_http_scheme(&decoded).then(|| Arc::from(decoded))
}

fn starts_http_scheme(text: &str) -> bool {
    text.get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || text
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn bare_url_end(text: &str, pos: usize) -> Option<usize> {
    if !starts_http_scheme_at(text, pos)
        || pos.checked_sub(1).is_some_and(|i| {
            text.as_bytes()[i].is_ascii_alphanumeric() || text.as_bytes()[i] == b'_'
        })
    {
        return None;
    }

    let mut at = pos;
    let mut parentheses = 0usize;
    for (relative, ch) in text[pos..].char_indices() {
        if ch.is_whitespace()
            || ch.is_control()
            || matches!(ch, '<' | '>' | '"' | '`' | '*' | '[' | ']')
        {
            break;
        }
        if relative + ch.len_utf8() > MAX_LINK_DESTINATION_BYTES {
            return None;
        }
        if ch == '(' {
            parentheses += 1;
        } else if ch == ')' {
            if parentheses == 0 {
                break;
            }
            parentheses -= 1;
        }
        at = pos + relative + ch.len_utf8();
    }
    while at > pos
        && text[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| matches!(ch, '.' | ',' | '!' | '?' | ';' | ':' | '\''))
    {
        at -= text[..at].chars().next_back().unwrap().len_utf8();
    }
    (at > pos).then_some(at)
}

fn starts_http_scheme_at(text: &str, pos: usize) -> bool {
    let Some(rest) = text.as_bytes().get(pos..) else {
        return false;
    };
    rest.get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
        || rest
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
}

struct InlineMath {
    content: Range<usize>,
    end: usize,
}

/// A run of digits with no LaTeX in sight is money, not maths: `$5 and $10`
/// must not become an equation. Requiring a LaTeX signal keeps `$2^n$`.
fn looks_like_currency(content: &str) -> bool {
    content.starts_with(|c: char| c.is_ascii_digit()) && !content.contains(['\\', '^', '_', '{'])
}

/// Recognises `$..$`, `$$..$$` and `\(..\)` starting at `pos`. Unmatched
/// delimiters return `None` and stay plain text, so a half-streamed equation
/// never mangles the line around it.
fn try_inline_math(text: &str, pos: usize) -> Option<InlineMath> {
    let bytes = text.as_bytes();
    let (open, close) = match bytes[pos] {
        b'$' if bytes.get(pos + 1) == Some(&b'$') => (MATH_FENCE, MATH_FENCE),
        b'$' => ("$", "$"),
        b'\\' if bytes.get(pos + 1) == Some(&b'(') => (MATH_PAREN_OPEN, MATH_PAREN_CLOSE),
        _ => return None,
    };

    let start = pos + open.len();
    let mut at = start;
    let end = loop {
        let found = at + text[at..].find(close)?;
        // A backslash escapes the delimiter, but `\\` is a literal backslash
        // and so does not escape what follows.
        let escaped = close == "$"
            && text[..found]
                .bytes()
                .rev()
                .take_while(|&b| b == b'\\')
                .count()
                .is_multiple_of(2)
                .not();
        if escaped {
            at = found + close.len();
            continue;
        }
        break found;
    };

    let content = &text[start..end];
    if content.trim().is_empty() || looks_like_currency(content) {
        return None;
    }
    Some(InlineMath {
        content: start..end,
        end: end + close.len(),
    })
}

enum InlineMatch {
    Found {
        emphasis: Emphasis,
        content_start: usize,
        close: usize,
        delim_len: usize,
    },
    /// `**` with no closer: skip past the whole open run, not just one byte,
    /// otherwise the second `*` would re-trigger a match.
    Skip(usize),
    None,
}

fn count_run(bytes: &[u8], pos: usize, ch: u8) -> usize {
    bytes[pos..].iter().take_while(|&&b| b == ch).count()
}

fn count_backtick_run(bytes: &[u8], pos: usize) -> usize {
    count_run(bytes, pos, b'`')
}

fn find_code_span_close(bytes: &[u8], pos: usize, run_len: usize) -> Option<(usize, usize, usize)> {
    let content_start = pos + run_len;
    let mut i = content_start;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let close_run = count_backtick_run(bytes, i);
            if close_run == run_len {
                return Some((content_start, i, i + run_len));
            }
            i += close_run;
        } else {
            i += 1;
        }
    }
    None
}

fn find_emphasis_close(bytes: &[u8], start: usize, delim: &[u8]) -> Option<usize> {
    let mut pos = start;
    while pos + delim.len() <= bytes.len() {
        if bytes[pos] == b'`' {
            let run = count_backtick_run(bytes, pos);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, pos, run) {
                pos = close_end;
            } else {
                pos += run;
            }
            continue;
        }
        if bytes[pos..].starts_with(delim) {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_italic_close(bytes: &[u8], start: usize, ch: u8) -> Option<usize> {
    let mut pos = start;
    while pos < bytes.len() {
        if bytes[pos] == b'`' {
            let run = count_backtick_run(bytes, pos);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, pos, run) {
                pos = close_end;
            } else {
                pos += run;
            }
            continue;
        }
        if bytes[pos] == ch {
            if (ch == b'*' && pos + 1 < bytes.len() && bytes[pos + 1] == b'*')
                || (pos > 0 && bytes[pos - 1] == ch)
            {
                pos += 1;
                continue;
            }
            if pos > start && !bytes[pos - 1].is_ascii_whitespace() {
                if ch == b'_' && pos + 1 < bytes.len() && is_word_char(bytes[pos + 1]) {
                    pos += 1;
                    continue;
                }
                return Some(pos);
            }
        }
        pos += 1;
    }
    None
}

fn is_valid_italic_open(bytes: &[u8], pos: usize) -> bool {
    if pos + 1 >= bytes.len() || bytes[pos + 1].is_ascii_whitespace() {
        return false;
    }
    let ch = bytes[pos];
    if ch == b'*' {
        if bytes[pos + 1] == b'*' {
            return false;
        }
        if pos > 0 && bytes[pos - 1] == b'*' {
            return false;
        }
        if pos > 0 && is_word_char(bytes[pos - 1]) {
            return false;
        }
    }
    if ch == b'_' && pos > 0 && is_word_char(bytes[pos - 1]) {
        return false;
    }
    true
}

fn is_valid_strike_open(bytes: &[u8], pos: usize) -> bool {
    if pos + 2 >= bytes.len() {
        return false;
    }
    if bytes[pos + 2] == b'~' {
        return false;
    }
    if pos > 0 && bytes[pos - 1] == b'~' {
        return false;
    }
    !bytes[pos + 2].is_ascii_whitespace()
}

fn find_strike_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while pos + 1 < bytes.len() {
        if bytes[pos] == b'`' {
            let run = count_backtick_run(bytes, pos);
            if let Some((_, _, close_end)) = find_code_span_close(bytes, pos, run) {
                pos = close_end;
            } else {
                pos += run;
            }
            continue;
        }
        if bytes[pos] == b'~' && bytes[pos + 1] == b'~' {
            if pos + 2 < bytes.len() && bytes[pos + 2] == b'~' {
                pos += 1;
                continue;
            }
            if pos > start && bytes[pos - 1] == b'~' {
                pos += 1;
                continue;
            }
            if pos > start && !bytes[pos - 1].is_ascii_whitespace() {
                return Some(pos);
            }
        }
        pos += 1;
    }
    None
}

/// A multi-character delimiter run opens only when content follows the whole
/// run with nothing between: `a ** b ** c` is four asterisks, not bold, and
/// `*** x ***` must not fall back to reading its own third star as content.
fn opens_run(bytes: &[u8], after: usize) -> bool {
    bytes.get(after).is_some_and(|b| !b.is_ascii_whitespace())
}

fn try_star_emphasis(bytes: &[u8], pos: usize) -> InlineMatch {
    let run = count_run(bytes, pos, b'*');
    if run >= 3
        && opens_run(bytes, pos + run)
        && let Some(close) = find_emphasis_close(bytes, pos + 3, b"***")
        && close > pos + 3
    {
        return InlineMatch::Found {
            emphasis: Emphasis::BOLD_ITALIC,
            content_start: pos + 3,
            close,
            delim_len: 3,
        };
    }
    if run >= 2 {
        if opens_run(bytes, pos + run)
            && let Some(close) = find_emphasis_close(bytes, pos + 2, b"**")
            && close > pos + 2
        {
            return InlineMatch::Found {
                emphasis: Emphasis::BOLD,
                content_start: pos + 2,
                close,
                delim_len: 2,
            };
        }
        return InlineMatch::Skip(2);
    }
    if is_valid_italic_open(bytes, pos)
        && let Some(close) = find_italic_close(bytes, pos + 1, b'*')
        && close > pos + 1
    {
        return InlineMatch::Found {
            emphasis: Emphasis::ITALIC,
            content_start: pos + 1,
            close,
            delim_len: 1,
        };
    }
    InlineMatch::Skip(1)
}

fn try_strike_emphasis(bytes: &[u8], pos: usize) -> InlineMatch {
    if pos + 1 >= bytes.len() || bytes[pos + 1] != b'~' {
        return InlineMatch::None;
    }
    if is_valid_strike_open(bytes, pos)
        && let Some(close) = find_strike_close(bytes, pos + 2)
        && close > pos + 2
    {
        return InlineMatch::Found {
            emphasis: Emphasis::STRIKE,
            content_start: pos + 2,
            close,
            delim_len: 2,
        };
    }
    InlineMatch::Skip(2)
}

fn try_underscore_emphasis(bytes: &[u8], pos: usize) -> InlineMatch {
    if !is_valid_italic_open(bytes, pos) {
        return InlineMatch::None;
    }
    if let Some(close) = find_italic_close(bytes, pos + 1, b'_')
        && close > pos + 1
    {
        return InlineMatch::Found {
            emphasis: Emphasis::ITALIC,
            content_start: pos + 1,
            close,
            delim_len: 1,
        };
    }
    InlineMatch::Skip(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn span_text(spans: &[InlineSpan]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn parse_inline_plain_text_yields_single_text_span() {
        let spans = parse_inline("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert_eq!(spans[0].kind, SpanKind::Text);
        assert!(spans[0].emphasis.is_empty());
    }

    #[test]
    fn parse_inline_bold_emits_bold_span_and_strips_delimiters() {
        let spans = parse_inline("a **b** c");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "a ");
        assert!(spans[0].emphasis.is_empty());
        assert_eq!(spans[1].text, "b");
        assert_eq!(spans[1].emphasis, Emphasis::BOLD);
        assert_eq!(spans[2].text, " c");
    }

    #[test_case("*x*"; "star")]
    #[test_case("_y_"; "underscore")]
    fn parse_inline_italic_variants(input: &str) {
        let spans = parse_inline(input);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].emphasis, Emphasis::ITALIC);
    }

    #[test]
    fn parse_inline_triple_star_is_bold_italic() {
        let spans = parse_inline("***hi***");
        assert_eq!(spans[0].emphasis, Emphasis::BOLD_ITALIC);
        assert_eq!(spans[0].text, "hi");
    }

    #[test]
    fn parse_inline_code_span_keeps_kind() {
        let spans = parse_inline("a `b()` c");
        assert_eq!(spans[1].kind, SpanKind::Code);
        assert_eq!(spans[1].text, "b()");
        assert!(spans[1].emphasis.is_empty());
    }

    #[test]
    fn parse_inline_strikethrough() {
        let spans = parse_inline("~~gone~~");
        assert_eq!(spans[0].emphasis, Emphasis::STRIKE);
        assert_eq!(spans[0].text, "gone");
    }

    #[test]
    fn parse_inline_code_inside_bold_preserves_both_axes() {
        let spans = parse_inline("**bold `code` bold**");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "bold ");
        assert_eq!(spans[0].emphasis, Emphasis::BOLD);
        assert_eq!(spans[0].kind, SpanKind::Text);
        assert_eq!(spans[1].text, "code");
        assert_eq!(spans[1].emphasis, Emphasis::BOLD);
        assert_eq!(spans[1].kind, SpanKind::Code);
        assert_eq!(spans[2].text, " bold");
        assert_eq!(spans[2].emphasis, Emphasis::BOLD);
    }

    #[test]
    fn parse_inline_markdown_link_uses_label_and_keeps_raw_target() {
        let input = "Read [the docs](https://example.com/path?q=1) now";
        let spans = parse_inline(input);
        assert_eq!(span_text(&spans), "Read the docs now");
        let link = spans.iter().find(|span| span.text == "the docs").unwrap();
        assert_eq!(link.link.as_deref(), Some("https://example.com/path?q=1"));
        assert_eq!(link.source, Source::atomic(5..45));
    }

    #[test]
    fn parse_inline_link_label_keeps_nested_styles_and_one_target() {
        let input = "[**bold** and `code`](https://example.com)";
        let spans = parse_inline(input);
        assert_eq!(span_text(&spans), "bold and code");
        assert_eq!(spans[0].emphasis, Emphasis::BOLD);
        assert_eq!(spans[2].kind, SpanKind::Code);
        assert!(
            spans
                .iter()
                .all(|span| span.link.as_deref() == Some("https://example.com"))
        );
        assert!(
            spans
                .iter()
                .all(|span| span.source == Source::atomic(0..42))
        );
    }

    #[test_case("[array[i]](https://example.com)" => "array[i]"; "nested_brackets")]
    #[test_case("[value `]` here](https://example.com)" => "value ] here"; "code_bracket")]
    #[test_case("[$x]$](https://example.com)" => "x]"; "math_bracket")]
    fn parse_inline_link_label_balances_brackets(input: &str) -> String {
        let spans = parse_inline(input);
        assert!(
            spans
                .iter()
                .all(|span| span.link.as_deref() == Some("https://example.com"))
        );
        span_text(&spans)
    }

    #[test]
    fn parse_inline_link_decodes_escaped_target_punctuation() {
        let spans = parse_inline(r"[site](https://example.com/a\(b\))");

        assert_eq!(spans[0].link.as_deref(), Some("https://example.com/a(b)"));
    }

    #[test_case("<https://example.com/a>", "https://example.com/a"; "autolink")]
    #[test_case("HTTPS://example.com/a", "HTTPS://example.com/a"; "uppercase_bare")]
    #[test_case("https://example.com/a.", "https://example.com/a"; "trailing_period")]
    #[test_case(
        "https://en.wikipedia.org/wiki/Function_(mathematics)",
        "https://en.wikipedia.org/wiki/Function_(mathematics)";
        "balanced_parentheses"
    )]
    fn parse_inline_visible_http_urls_are_links(input: &str, expected: &str) {
        let spans = parse_inline(input);
        let link = spans.iter().find(|span| span.link.is_some()).unwrap();
        assert_eq!(link.text, expected);
        assert_eq!(link.link.as_deref(), Some(expected));
    }

    #[test]
    fn parse_inline_rejects_overlong_visible_urls() {
        let target = format!(
            "https://example.com/{}",
            "a".repeat(MAX_LINK_DESTINATION_BYTES)
        );

        assert!(parse_inline(&target).iter().all(|span| span.link.is_none()));
        assert!(
            parse_inline(&format!("<{target}>"))
                .iter()
                .all(|span| span.link.is_none())
        );
    }

    #[test]
    fn parse_inline_parenthesized_bare_url_drops_prose_closer() {
        let spans = parse_inline("See (https://example.com/path). Then");
        let link = spans.iter().find(|span| span.link.is_some()).unwrap();
        assert_eq!(link.text, "https://example.com/path");
        assert_eq!(span_text(&spans), "See (https://example.com/path). Then");
    }

    #[test]
    fn parse_inline_relative_link_renders_label_but_is_not_clickable() {
        let spans = parse_inline("[local](../README.md)");
        assert_eq!(span_text(&spans), "local");
        assert!(spans.iter().all(|span| span.link.is_none()));
        assert_eq!(spans[0].source, Source::atomic(0..21));
    }

    #[test]
    fn parse_inline_link_title_is_not_part_of_target() {
        let spans = parse_inline("[site](https://example.com \"Example\")");
        assert_eq!(span_text(&spans), "site");
        assert_eq!(spans[0].link.as_deref(), Some("https://example.com"));
    }

    #[test_case("`https://example.com`"; "code")]
    #[test_case("$https://example.com$"; "math")]
    #[test_case("![alt](https://example.com/image.png)"; "image")]
    fn parse_inline_non_link_constructs_do_not_expose_urls(input: &str) {
        assert!(parse_inline(input).iter().all(|span| span.link.is_none()));
    }

    #[test]
    fn parse_inline_bold_inside_code_treats_code_as_atomic() {
        let spans = parse_inline("`code **bold** code`");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "code **bold** code");
        assert_eq!(spans[0].kind, SpanKind::Code);
    }

    #[test]
    fn parse_inline_nested_bold_inside_italic_becomes_bold_italic() {
        let spans = parse_inline("*a **b** c*");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].emphasis, Emphasis::ITALIC);
        assert_eq!(spans[1].emphasis, Emphasis::BOLD_ITALIC);
        assert_eq!(spans[2].emphasis, Emphasis::ITALIC);
    }

    #[test_case(1, "# h1"; "level_1")]
    #[test_case(2, "## h2"; "level_2")]
    #[test_case(3, "### h3"; "level_3")]
    #[test_case(4, "#### h4"; "level_4")]
    #[test_case(5, "##### h5"; "level_5")]
    #[test_case(6, "###### h6"; "level_6")]
    fn parse_heading_levels_1_through_6(level: u8, input: &str) {
        let (got, content, _) = parse_heading(input).expect("heading parses");
        assert_eq!(got, level);
        assert_eq!(content, format!("h{level}"));
    }

    #[test]
    fn parse_seven_hashes_is_not_heading() {
        assert!(parse_heading("####### nope").is_none());
    }

    #[test]
    fn parse_horizontal_rule_classifies_as_hr() {
        let blocks = parse("---");
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(lines[0].kind, BlockKind::HorizontalRule);
    }

    #[test]
    fn parse_unordered_list_records_depth() {
        let blocks = parse("- item\n  - nested");
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(lines[0].kind, BlockKind::UnorderedListItem { depth: 0 });
        assert_eq!(lines[1].kind, BlockKind::UnorderedListItem { depth: 1 });
    }

    #[test]
    fn parse_preserves_user_newlines_one_logical_line_each() {
        let blocks = parse("a\nb\nc");
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].inline, "a");
        assert_eq!(lines[2].inline, "c");
    }

    #[test]
    fn parse_fenced_code_block_emits_code_block() {
        let blocks = parse("```rust\nfn x() {}\nlet y;\n```");
        assert_eq!(blocks.len(), 1);
        let Block::Code { lang, code, .. } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(lang, "rust");
        assert_eq!(code, "fn x() {}\nlet y;");
    }

    #[test]
    fn parse_table_emits_table_block_with_header_separator_dropped() {
        let blocks = parse("| Name | Value |\n| --- | --- |\n| foo | 42 |");
        assert_eq!(blocks.len(), 1);
        let Block::Table {
            rows, header_end, ..
        } = &blocks[0]
        else {
            panic!("expected Table")
        };
        assert_eq!(*header_end, 1);
        assert_eq!(rows, &[vec!["Name", "Value"], vec!["foo", "42"]]);
    }

    #[test]
    fn parse_inline_never_panics_on_arbitrary_unicode() {
        let mut rng = fastrand::Rng::with_seed(0xC0FFEE);
        for _ in 0..500 {
            let n = rng.usize(0..200);
            let mut s = String::with_capacity(n);
            for _ in 0..n {
                s.push(rng.char(..));
            }
            let spans = parse_inline(&s);
            let total: usize = spans.iter().map(|sp| sp.text.len()).sum();
            assert!(total <= s.len(), "fabrication: {} > {}", total, s.len());
        }
    }

    #[test]
    fn parse_inline_visible_text_invariant() {
        let cases = [
            "plain text",
            "a **b** c",
            "a *b* c",
            "a `code` b",
            "a ~~strike~~ b",
        ];
        for input in cases {
            let spans = parse_inline(input);
            let visible = span_text(&spans);
            let strip = |s: &str| -> String {
                s.chars()
                    .filter(|c| !matches!(c, '`' | '*' | '~' | '_'))
                    .collect()
            };
            assert_eq!(strip(&visible), strip(input), "input: {input:?}");
        }
    }

    #[test]
    fn parse_inline_empty_input_returns_empty() {
        assert!(parse_inline("").is_empty());
    }

    #[test]
    fn parse_inline_unmatched_star_passes_through_as_plain() {
        let spans = parse_inline("a*b");
        assert!(
            spans
                .iter()
                .all(|s| s.kind == SpanKind::Text && s.emphasis.is_empty())
        );
        assert_eq!(span_text(&spans), "a*b");
    }

    #[test_case("- item", Some("• "); "unordered_depth_zero")]
    #[test_case("  - item", Some("  • "); "unordered_depth_one")]
    fn block_prefix_unordered_list_depth(input: &str, expected: Option<&str>) {
        let blocks = parse(input);
        let Block::Lines(lines) = &blocks[0] else {
            panic!("expected Lines")
        };
        assert_eq!(block_prefix(&lines[0].kind).as_deref(), expected);
    }

    const STRIP_DELIMS: &[char] = &['`', '*', '~', '_'];

    fn first_lines(blocks: &[Block]) -> &[LineBlock] {
        match &blocks[0] {
            Block::Lines(l) => l,
            other => panic!("expected Lines, got {other:?}"),
        }
    }

    #[test]
    fn emphasis_merge_ors_fields_and_default_is_identity() {
        assert_eq!(
            Emphasis::BOLD.merge(Emphasis::ITALIC),
            Emphasis::BOLD_ITALIC
        );
        let e = Emphasis::BOLD_ITALIC;
        assert_eq!(e.merge(Emphasis::default()), e);
        assert!(Emphasis::default().is_empty());
        assert!(!Emphasis::BOLD.is_empty());
    }

    #[test]
    fn ordered_list_depth_and_marker_preserved() {
        let lines = first_lines(&parse("1. a\n   2. nested\n10. ten")).to_vec();
        assert_eq!(
            lines[0].kind,
            BlockKind::OrderedListItem {
                depth: 0,
                marker: "1.".to_owned()
            }
        );
        // 3 spaces over a 2-space step rounds down to depth 1.
        assert_eq!(
            lines[1].kind,
            BlockKind::OrderedListItem {
                depth: 1,
                marker: "2.".to_owned()
            }
        );
        assert_eq!(
            lines[2].kind,
            BlockKind::OrderedListItem {
                depth: 0,
                marker: "10.".to_owned()
            }
        );
    }

    #[test_case(0, "1.", "1. "; "depth_zero")]
    #[test_case(2, "42.", "    42. "; "depth_two_two_digits")]
    fn block_prefix_ordered_list_depths(depth: usize, marker: &str, expected: &str) {
        let kind = BlockKind::OrderedListItem {
            depth,
            marker: marker.to_owned(),
        };
        assert_eq!(block_prefix(&kind).as_deref(), Some(expected));
    }

    #[test_case("---"; "three_dashes")]
    #[test_case("***"; "three_stars")]
    #[test_case("___"; "three_unders")]
    #[test_case("- - -"; "spaced_dashes")]
    #[test_case("* * *"; "spaced_stars")]
    #[test_case("-- -"; "two_dashes_space_dash_still_three_dashes_total")]
    fn horizontal_rule_accepted_variants(input: &str) {
        assert_eq!(
            first_lines(&parse(input))[0].kind,
            BlockKind::HorizontalRule
        );
    }

    #[test_case("--"; "two_dashes")]
    #[test_case("-a-"; "letter_between")]
    #[test_case("**a"; "two_stars_letter")]
    #[test_case("---x"; "trailing_non_marker")]
    fn horizontal_rule_rejected_variants(input: &str) {
        assert_ne!(
            first_lines(&parse(input))[0].kind,
            BlockKind::HorizontalRule
        );
    }

    #[test]
    fn heading_requires_space_or_empty_after_hashes() {
        assert!(parse_heading("#nospace").is_none());
    }

    #[test_case("#"; "bare_hash")]
    #[test_case("# "; "hash_space")]
    fn bare_hash_is_heading_level_1_with_empty_inline(input: &str) {
        let blocks = parse(input);
        let lb = &first_lines(&blocks)[0];
        assert_eq!(lb.kind, BlockKind::Heading(1));
        assert_eq!(lb.inline, "");
    }

    const FOUR_BACKTICK_FENCE: &str = "````\n```\ninner\n```\n````";

    #[test]
    fn four_backtick_fence_wraps_inner_three_backtick_block() {
        let blocks = parse(FOUR_BACKTICK_FENCE);
        let Block::Code { lang, code, .. } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(lang, "");
        assert_eq!(code, "```\ninner\n```");
    }

    #[test]
    fn unclosed_code_fence_runs_to_eof() {
        let blocks = parse("```\nfoo\nbar");
        let Block::Code { code, .. } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(code, "foo\nbar");
    }

    #[test]
    fn mid_line_backticks_do_not_open_fence() {
        // A run of backticks only opens a fence at the start of a line.
        let blocks = parse("text ``` text");
        let lb = &first_lines(&blocks)[0];
        assert_eq!(lb.kind, BlockKind::Paragraph);
        assert_eq!(lb.inline, "text ``` text");
    }

    #[test]
    fn code_fence_language_is_optional() {
        let blocks = parse("```\ncode\n```");
        let Block::Code { lang, code, .. } = &blocks[0] else {
            panic!("expected Code")
        };
        assert_eq!(lang, "");
        assert_eq!(code, "code");
    }

    #[test]
    fn underscore_italic_does_not_match_intraword() {
        let spans = parse_inline("foo_bar_baz");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "foo_bar_baz");
        assert!(spans[0].emphasis.is_empty());
    }

    #[test]
    fn star_italic_intraword_does_not_open_when_preceded_by_word_char() {
        // A word char before `*` blocks italic from opening, so `a*b*c`
        // stays plain. Worth a test because file names like `a*b*c` are real.
        let spans = parse_inline("a*b*c");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "a*b*c");
        assert!(spans[0].emphasis.is_empty());
    }

    #[test_case("~not~"; "single_tildes")]
    #[test_case("~~foo"; "unclosed_double")]
    fn strikethrough_non_match_stays_plain(input: &str) {
        let spans = parse_inline(input);
        assert!(
            spans
                .iter()
                .all(|s| s.emphasis.is_empty() && s.kind == SpanKind::Text)
        );
        assert_eq!(span_text(&spans), input);
    }

    #[test]
    fn code_span_with_pipes_and_emphasis_chars_is_atomic() {
        let spans = parse_inline("a `x|y*z` b");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].kind, SpanKind::Code);
        assert_eq!(spans[1].text, "x|y*z");
        assert!(spans[1].emphasis.is_empty());
    }

    #[test]
    fn table_cell_backslash_pipe_is_literal_pipe() {
        let blocks = parse("| a | b\\|c | d |\n| --- | --- | --- |\n| 1 | 2 | 3 |");
        let Block::Table {
            rows, header_end, ..
        } = &blocks[0]
        else {
            panic!("expected Table")
        };
        assert_eq!(*header_end, 1);
        assert_eq!(rows[0], vec!["a", "b|c", "d"]);
    }

    #[test]
    fn table_cell_backticked_pipe_stays_inside_cell() {
        let blocks = parse("| `x|y` | z |\n|---|---|");
        let Block::Table { rows, .. } = &blocks[0] else {
            panic!("expected Table")
        };
        assert_eq!(rows[0], vec!["`x|y`", "z"]);
    }

    #[test]
    fn underscore_italic_with_nested_bold_becomes_bold_italic() {
        let spans = parse_inline("_a **b** c_");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].emphasis, Emphasis::ITALIC);
        assert_eq!(spans[1].emphasis, Emphasis::BOLD_ITALIC);
        assert_eq!(spans[1].text, "b");
        assert_eq!(spans[2].emphasis, Emphasis::ITALIC);
    }

    #[test]
    fn triple_star_mismatched_close_preserves_visible_text() {
        let input = "***bold only**";
        let spans = parse_inline(input);
        let visible = span_text(&spans);
        let strip =
            |s: &str| -> String { s.chars().filter(|c| !STRIP_DELIMS.contains(c)).collect() };
        assert_eq!(strip(&visible), strip(input));
    }

    #[test_case("a ** b ** c"       ; "spaced_double")]
    #[test_case("2 ** 3 ** 4"       ; "exponent")]
    #[test_case("*** x ***"         ; "spaced_triple")]
    fn a_star_run_needs_content_to_open(input: &str) {
        let spans = parse_inline(input);
        assert!(
            spans.iter().all(|s| s.emphasis.is_empty()),
            "{input} must stay literal, got {spans:?}"
        );
    }

    const SETTLED: &[&str] = &[
        "plain words",
        "a **b** c",
        "*i* and _j_ and ~~k~~",
        "a `code()` span",
        "see [docs](https://example.com) now",
        "# heading with **bold**",
        "- item one\n- item two",
        "5 * 3 and a_b_c and 50% ** off",
        "```rust\nfn x() {}\n```\ndone",
        "| a | b |\n| --- | --- |\n| 1 | 2 |",
    ];

    #[test]
    fn close_open_tail_leaves_settled_markdown_untouched() {
        for text in SETTLED {
            let out = close_open_tail(text);
            assert!(
                matches!(out, Cow::Borrowed(_)),
                "{text:?} must not be rewritten, got {out:?}"
            );
            assert_eq!(out, *text);
        }
    }

    #[test_case("**bold",            "**bold**"              ; "double_star")]
    #[test_case("***both",           "***both***"            ; "triple_star")]
    #[test_case("a *ital",           "a *ital*"              ; "single_star")]
    #[test_case("a _ital",           "a _ital_"              ; "underscore")]
    #[test_case("~~gone",            "~~gone~~"              ; "strike")]
    #[test_case("a `code",           "a `code`"              ; "code_span")]
    #[test_case("a ``co`de",         "a ``co`de``"           ; "code_span_double_run")]
    #[test_case("## **bo",           "## **bo**"             ; "inside_heading")]
    #[test_case("- **bo",            "- **bo**"              ; "inside_list_item")]
    #[test_case("| a | **bo",        "| a | **bo**"          ; "inside_table_row")]
    #[test_case("done\n\n**bo",      "done\n\n**bo**"        ; "after_a_blank_line")]
    #[test_case("```\nx\n```\n**bo", "```\nx\n```\n**bo**"   ; "after_a_closed_fence")]
    #[test_case("see [d](h",         "see [d](h)"            ; "link_destination")]
    #[test_case("a <https://ex",     "a <https://ex>"        ; "autolink")]
    fn close_open_tail_closes_what_the_tail_left_open(input: &str, expected: &str) {
        assert_eq!(close_open_tail(input), expected);
    }

    #[test_case("a *",        "a "    ; "lone_star")]
    #[test_case("a **",       "a "    ; "lone_double_star")]
    #[test_case("a ***",      "a "    ; "lone_triple_star")]
    #[test_case("a ~~",       "a "    ; "lone_strike")]
    #[test_case("a `",        "a "    ; "lone_backtick")]
    #[test_case("a **bold*",  "a **bold**" ; "half_arrived_closer")]
    #[test_case("see [",      "see "  ; "link_open_bracket")]
    #[test_case("see [do",    "see "  ; "link_partial_label")]
    #[test_case("see [docs]", "see "  ; "link_label_only")]
    #[test_case("see [docs](", "see " ; "link_empty_destination")]
    #[test_case("an ![alt](",  "an "  ; "image_empty_destination")]
    fn close_open_tail_holds_back_what_it_cannot_classify_yet(input: &str, expected: &str) {
        assert_eq!(close_open_tail(input), expected);
    }

    #[test_case("```rust\nfn x() {"     ; "open_code_fence")]
    #[test_case("$$\nx = *y"            ; "open_math_fence")]
    #[test_case("---"                   ; "horizontal_rule")]
    #[test_case("5 * 3"                 ; "arithmetic")]
    #[test_case("a ** b"                ; "spaced_double_star")]
    #[test_case("snake_case_ident"      ; "intra_word_underscore")]
    #[test_case("if a < b"              ; "less_than")]
    #[test_case("* item"                ; "bullet_marker")]
    #[test_case("`**kwargs`"            ; "markers_inside_code")]
    fn close_open_tail_leaves_a_settled_tail_alone(input: &str) {
        assert_eq!(close_open_tail(input), input);
    }

    /// The point of the whole exercise: a reader never sees a delimiter that
    /// is about to be taken away, and content never shifts once it is drawn.
    #[test_case("a **bold** c"                    ; "bold")]
    #[test_case("a *ital* c"                      ; "italic")]
    #[test_case("a _ital_ c"                      ; "underscore_italic")]
    #[test_case("a ***both*** c"                  ; "bold_italic")]
    #[test_case("a ~~gone~~ c"                    ; "strike")]
    #[test_case("a `code()` c"                    ; "code_span")]
    #[test_case("see [docs](https://example.com)" ; "link")]
    #[test_case("## a **bold** heading"           ; "heading")]
    fn no_streaming_prefix_draws_a_delimiter_it_will_take_back(text: &str) {
        let settled: String = span_text(&parse_inline(
            &close_open_tail(text)[classify_line(text, 0).inline_start as usize..],
        ));
        let mut drawn = String::new();
        for end in 1..=text.len() {
            if !text.is_char_boundary(end) {
                continue;
            }
            let normalized = close_open_tail(&text[..end]);
            let inline_start = classify_line(&normalized, 0).inline_start as usize;
            let visible = span_text(&parse_inline(&normalized[inline_start..]));
            assert!(
                !visible.contains(STRIP_DELIMS),
                "prefix {:?} drew a delimiter: {visible:?}",
                &text[..end]
            );
            assert!(
                visible.starts_with(&drawn),
                "prefix {:?} rewrote {drawn:?} as {visible:?}",
                &text[..end]
            );
            drawn = visible;
        }
        assert_eq!(drawn, settled);
    }
}
