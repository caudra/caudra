use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::highlight::{fallback_span, highlight_line};
use crate::markdown::{expand_notice, should_truncate, text_to_painted, truncation_notice};
use crate::provenance::LineProvenance;
use crate::theme;

use super::tool_display::{
    ScrollTail, batch_sigil_style, compact_args_for, compact_sigil_label, header_spans, names_tool,
    scroll_footer_text,
};
use super::{ToolProgress, is_collapsible, workflow_card};
use caudra_agent::tools::{PYTHON_EXECUTION_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME};
use caudra_agent::types::Answer;
use caudra_agent::types::{TodoItem, TodoStatus};
use caudra_agent::{
    BatchToolEntry, BatchToolStatus, CodeGraphRow, CodeGraphSource, GrepFileEntry, INDEX_TRUNCATED,
    IndexDirectoryEntryKind, IndexLine, IndexLineSemantic, IndexOutput, IndexSourceRange,
    InstructionBlock, PatchedFile, SearchCap, SubagentProgress, ToolInput, ToolOutput,
};
use caudra_config::ToolOutputLines;
use caudra_diff::{DiffHunk, DiffLine, DiffSpan, compute_hunks};
use caudra_markdown::Source;
use caudra_markdown::render::SpanSource;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::parsing::SyntaxReference;
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthStr;

pub(crate) const MAX_INSTRUCTION_LINES: usize = 15;
const BATCH_CHILD_INDENT: &str = "  ";
const BATCH_CHILD_INDENT_WIDTH: u16 = BATCH_CHILD_INDENT.len() as u16;
const ANSWER_MARK: &str = "  \u{2713} ";
const ANSWER_INDENT: &str = "    ";
const NO_ANSWER: &str = "(no answer)";
/// Indented past the child's own sigil, so the activity reads as belonging to
/// the row above it rather than as another entry in the roster.
const CHILD_ACTIVITY_PREFIX: &str = "  ├ ";
const CHILD_ACTIVITY_SEPARATOR: &str = " · ";
/// Says a child is folded, so a row with nothing under it is not mistaken for
/// one whose body was hidden.
const BATCH_FOLDED_MARK: &str = " \u{2026}";
const QUEUED_ANNOTATION: &str = "queued";
/// What the markdown renderer calls a width it should not wrap to.
const UNCONSTRAINED_WIDTH: u16 = 0;
const GREP_COUNT_SEP: &str = " \u{b7} ";
const GREP_SUMMARY_INDENT: &str = "  ";
/// Past this many lines in one hunk, diffing its two sides again costs more
/// than the grouping it buys, so the wire's own order is drawn instead. A card
/// is rebuilt on every resize and theme change, so that cost is paid again
/// each time.
const MAX_REDIFF_LINES: usize = 4096;
/// The columns a diff body needs before a second gutter of line numbers is
/// worth what it takes away from the code.
const MIN_CODE_COLUMNS: usize = 60;
const MARK_UNCHANGED: char = ' ';
const MARK_REMOVED: char = '-';
const MARK_ADDED: char = '+';
/// A line the alignment matched whose whitespace moved. Neither side of it is
/// new, so neither `-` nor `+` describes it.
const MARK_REINDENTED: char = '~';
/// Parts a card's source blocks by the blank row drawn between them, so a
/// selection spanning both copies the gap it saw rather than closing it.
const BLOCK_GAP: &str = "\n\n";

pub(crate) fn instruction_limit(expanded: bool) -> usize {
    if expanded {
        usize::MAX
    } else {
        MAX_INSTRUCTION_LINES
    }
}

fn nr_width(max_nr: usize) -> usize {
    max_nr.max(1).ilog10() as usize + 1
}

fn gutter(nr_str: &str) -> Span<'static> {
    Span::styled(format!("{nr_str} "), theme::current().diff_line_nr)
}

fn gap_ellipsis() -> Line<'static> {
    Line::from(vec![
        Span::styled("...".to_owned(), theme::current().tool_dim),
        Span::raw("  ".to_owned()),
    ])
}

fn truncation_line(truncated: usize) -> Line<'static> {
    Line::from(Span::styled(
        truncation_notice(truncated),
        theme::current().tool_dim,
    ))
}

fn highlight_spans(hl: &mut caudra_highlight::Highlighter, text: &str) -> Vec<Span<'static>> {
    let with_nl = format!("{text}\n");
    highlight_line(hl, &with_nl)
        .into_iter()
        .filter(|span| !span.content.is_empty())
        .collect()
}

/// The text a body was painted from, with one entry per painted row, so copy
/// slices bytes instead of reading the glyphs back off the screen. Without it
/// a code row copies with the line-number gutter it is drawn behind.
#[derive(Default, Clone)]
pub struct BodySource {
    pub text: String,
    pub rows: Vec<LineProvenance>,
    /// The rows holding source code, and what to call it, so a copy that ran
    /// past the block can fence it rather than drop code into prose.
    pub code: Option<CodeBlock>,
}

/// Where a card's code sits among its painted rows, and the language a fence
/// around it should name.
#[derive(Clone, Debug)]
pub struct CodeBlock {
    pub rows: Range<usize>,
    pub language: Option<String>,
}

impl BodySource {
    /// Prepends a chrome span to every row, for a body whose lines are about
    /// to be indented into a card.
    pub fn indented(mut self) -> Self {
        for row in &mut self.rows {
            row.spans.insert(0, SpanSource::Chrome);
        }
        self
    }

    /// Whether any row names source at all. A card where none does has to copy
    /// by scraping, or every row of it would reach the clipboard as nothing.
    pub fn names_source(&self) -> bool {
        self.rows.iter().any(|row| row.line.is_some())
    }

    /// Names the code block after the language a tool declared for it.
    fn named(mut self, language: &str) -> Self {
        if let Some(code) = self.code.as_mut() {
            code.language = Some(language.to_owned());
        }
        self
    }

    /// Names the code block after the file it was read from, which is all a
    /// path-addressed body says about its language.
    fn named_for_path(self, path: &str) -> Self {
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str());
        match extension {
            Some(extension) => self.named(extension),
            None => self,
        }
    }

    fn push_chrome(&mut self, span_count: usize) {
        self.rows.push(LineProvenance::chrome(span_count));
    }
}

/// Gathers a card's source block by block as it is painted, and marks the rows
/// no renderer claimed as chrome once the line count has settled.
///
/// Deferring that fill is what keeps rows aligned with lines: a later pass
/// inserts spans into lines already recorded — the status indicator on row
/// zero — and a span count taken at push time would be wrong by then.
#[derive(Default)]
pub struct SourceTrace {
    text: String,
    rows: Vec<Option<LineProvenance>>,
    code: Option<CodeBlock>,
    abandoned: bool,
}

impl SourceTrace {
    /// Places a body's rows at `start`, the line its first row landed on,
    /// rebasing its ranges onto the text gathered so far.
    pub fn record(&mut self, start: usize, mut body: BodySource) {
        if !self.text.is_empty() && !body.text.is_empty() {
            self.text.push_str(BLOCK_GAP);
        }
        let base = self.text.len() as u32;
        self.text.push_str(&body.text);
        for row in &mut body.rows {
            rebase_row(row, base);
        }
        if let Some(mut code) = body.code {
            code.rows.start += start;
            code.rows.end += start;
            self.code.get_or_insert(code);
        }
        self.rows.resize(start, None);
        self.rows.extend(body.rows.into_iter().map(Some));
    }

    /// Gives the card up to the scraping fallback, for a body drawn by
    /// something that records no source to slice.
    pub fn abandon(&mut self) {
        self.abandoned = true;
    }

    /// One row per painted line, with everything unclaimed marked as chrome.
    pub fn finish(mut self, lines: &[Line<'static>]) -> Option<BodySource> {
        if self.abandoned {
            return None;
        }
        self.rows.resize(lines.len(), None);
        let rows = lines
            .iter()
            .zip(self.rows)
            .map(|(line, row)| row.unwrap_or_else(|| LineProvenance::chrome(line.spans.len())))
            .collect();
        Some(BodySource {
            text: self.text,
            rows,
            code: self.code,
        })
    }
}

/// Text drawn one raw line per row behind `chrome_spans` of padding, which is
/// how a card prints output no renderer claimed.
pub fn text_body(text: &str, chrome_spans: usize) -> BodySource {
    let mut at = 0u32;
    let rows = text
        .lines()
        .map(|line| {
            let range = at..at + line.len() as u32;
            at = range.end + 1;
            let mut spans = vec![SpanSource::Chrome; chrome_spans];
            spans.push(SpanSource::Range(Source::verbatim(range.clone())));
            LineProvenance {
                line: Some(range),
                spans,
            }
        })
        .collect();
    BodySource {
        text: text.to_owned(),
        rows,
        code: None,
    }
}

/// Moves a row's ranges onto text that now sits `by` bytes further in.
fn rebase_row(row: &mut LineProvenance, by: u32) {
    if let Some(range) = row.line.as_mut() {
        range.start += by;
        range.end += by;
    }
    for span in &mut row.spans {
        if let SpanSource::Range(source) = span {
            source.range.start += by;
            source.range.end += by;
        }
    }
}

/// The source behind one painted code row. The gutter names nothing, and the
/// rest is a slice of the line the row was drawn from.
///
/// Tabs expand and trailing newlines vanish on the way to the screen, so a
/// line that does not survive that round trip is marked atomic: its glyph
/// offsets no longer count its bytes, and any touch of it copies it whole.
fn code_row(spans: &[Span<'static>], text: &str, at: u32) -> LineProvenance {
    let line = at..at + text.len() as u32;
    let verbatim = caudra_highlight::normalize_text(text) == text;
    let mut sources = vec![SpanSource::Chrome];
    let mut offset = at;
    for span in spans.iter().skip(1) {
        let end = offset + span.content.len() as u32;
        sources.push(SpanSource::Range(match verbatim {
            true => Source::verbatim(offset..end),
            false => Source::atomic(line.clone()),
        }));
        offset = end;
    }
    LineProvenance {
        line: Some(line),
        spans: sources,
    }
}

struct CodeRender {
    lines: Vec<Line<'static>>,
    truncated: bool,
    source: BodySource,
}

fn render_code(
    mut hl: Option<caudra_highlight::Highlighter>,
    start_line: usize,
    code_lines: &[String],
    total_count: usize,
    max_lines: usize,
) -> CodeRender {
    let capped = code_lines.len().min(max_lines);
    let hidden = total_count.saturating_sub(capped);
    let truncated = should_truncate(hidden);
    let display_count = if truncated { capped } else { code_lines.len() };
    let max_nr = start_line + display_count.saturating_sub(1);
    let w = nr_width(max_nr);

    let shown = &code_lines[..display_count.min(code_lines.len())];
    let mut source = BodySource {
        text: shown.join("\n"),
        rows: Vec::with_capacity(shown.len()),
        code: None,
    };
    let mut at = 0u32;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(shown.len());
    for (i, text) in shown.iter().enumerate() {
        let nr = start_line + i;
        let mut spans = vec![gutter(&format!("{nr:>w$}"))];
        match &mut hl {
            Some(h) => spans.extend(highlight_spans(h, text)),
            None => spans.push(fallback_span(text)),
        }
        source.rows.push(code_row(&spans, text, at));
        at += text.len() as u32 + 1;
        lines.push(Line::from(spans));
    }
    source.code = Some(CodeBlock {
        rows: 0..lines.len(),
        language: None,
    });

    if truncated {
        lines.push(truncation_line(hidden));
        source.push_chrome(1);
    }
    CodeRender {
        lines,
        truncated,
        source,
    }
}

/// Syntect is stateful, so to color line N you need lines 1..N first.
/// A diff has two files, each with its own parser state. We keep one
/// walker per side and step them in lockstep with the hunks.
struct FileWalker<'a> {
    lines: LinesWithEndings<'a>,
    pos: usize,
    hl: caudra_highlight::Highlighter,
}

impl<'a> FileWalker<'a> {
    fn new(content: &'a str, syntax: &'static SyntaxReference) -> Self {
        Self {
            lines: LinesWithEndings::from(content),
            pos: 1,
            hl: caudra_highlight::Highlighter::for_syntax(syntax),
        }
    }

    /// Advances the parser without keeping styled output (for lines we
    /// skip over in the diff). Debug-asserts if we run past EOF.
    fn skip(&mut self) -> bool {
        let Some(line) = self.lines.next() else {
            debug_assert!(
                false,
                "FileWalker::skip called past EOF at pos {}",
                self.pos
            );
            return false;
        };
        self.hl.advance(line);
        self.pos += 1;
        true
    }

    fn highlight_next(&mut self) -> Option<Vec<Span<'static>>> {
        let Some(line) = self.lines.next() else {
            debug_assert!(
                false,
                "FileWalker::highlight_next called past EOF at pos {}",
                self.pos
            );
            return None;
        };
        let spans = highlight_line(&mut self.hl, line);
        self.pos += 1;
        Some(spans)
    }

    fn skip_to(&mut self, target: usize) {
        while self.pos < target {
            if !self.skip() {
                return;
            }
        }
        debug_assert_eq!(
            self.pos, target,
            "FileWalker overshot or failed to reach target",
        );
    }
}

/// How a diff row's gutter is laid out. Two columns of numbers say which line
/// each side of a change sits on, the way a side-by-side diff does, but they
/// cost the code the room to read; a narrow card gets one column instead and
/// leans on the marker to say which side the number belongs to.
#[derive(Clone, Copy)]
struct DiffGutter {
    before_width: usize,
    after_width: Option<usize>,
}

impl DiffGutter {
    fn new(before_max: usize, after_max: usize, width: u16) -> Self {
        let (before, after) = (nr_width(before_max), nr_width(after_max));
        let two_columns = before + after + 4;
        let fits = width != UNCONSTRAINED_WIDTH
            && usize::from(width) >= two_columns.saturating_add(MIN_CODE_COLUMNS);
        match fits {
            true => Self {
                before_width: before,
                after_width: Some(after),
            },
            false => Self {
                before_width: nr_width(before_max.max(after_max)),
                after_width: None,
            },
        }
    }

    fn span(
        self,
        before: Option<usize>,
        after: Option<usize>,
        mark: char,
        base: Style,
    ) -> Span<'static> {
        let number = |nr: Option<usize>, width: usize| match nr {
            Some(nr) => format!("{nr:>width$}"),
            None => " ".repeat(width),
        };
        let text = match self.after_width {
            Some(after_width) => format!(
                "{} {} {mark} ",
                number(before, self.before_width),
                number(after, after_width)
            ),
            None => format!("{} {mark} ", number(before.or(after), self.before_width)),
        };
        Span::styled(text, base.patch(theme::current().diff_line_nr))
    }
}

/// The last line number each side reaches, which is what the gutter is sized
/// against.
fn hunk_extents(hunks: &[DiffHunk]) -> (usize, usize) {
    hunks.iter().fold((1, 1), |(before, after), hunk| {
        let (before_rows, after_rows) = hunk.lines.iter().fold((0, 0), |(b, a), line| {
            let (on_before, on_after) = line.sides();
            (b + usize::from(on_before), a + usize::from(on_after))
        });
        (
            before.max(hunk.before_start + before_rows.saturating_sub(1)),
            after.max(hunk.after_start + after_rows.saturating_sub(1)),
        )
    })
}

fn render_diff(
    syntax: Option<&'static SyntaxReference>,
    before: &str,
    after: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let hunks = compute_hunks(before, after);
    if hunks.is_empty() {
        return Vec::new();
    }
    let (before_max, after_max) = hunk_extents(&hunks);
    let gutter = DiffGutter::new(before_max, after_max, width);

    let mut walkers = syntax.map(|s| (FileWalker::new(before, s), FileWalker::new(after, s)));

    let mut lines = Vec::new();
    for (i, hunk) in hunks.iter().enumerate() {
        if i > 0 {
            lines.push(gap_ellipsis());
        }
        if let Some((before, after)) = walkers.as_mut() {
            before.skip_to(hunk.before_start);
            after.skip_to(hunk.after_start);
        }

        let mut cursor = (hunk.before_start, hunk.after_start);
        for dl in &hunk.lines {
            lines.push(render_hunk_line(
                dl,
                walkers.as_mut(),
                &mut cursor,
                gutter,
                width,
            ));
        }
    }

    lines
}

/// Unchanged and re-indented lines step both walkers but take spans from
/// `after`. Removed and added lines only step their own side.
fn render_hunk_line(
    dl: &DiffLine,
    walkers: Option<&mut (FileWalker<'_>, FileWalker<'_>)>,
    cursor: &mut (usize, usize),
    gutter: DiffGutter,
    width: u16,
) -> Line<'static> {
    let theme = theme::current();
    let (before_nr, after_nr) = *cursor;
    let (on_before, on_after) = dl.sides();
    cursor.0 += usize::from(on_before);
    cursor.1 += usize::from(on_after);
    let numbers = (on_before.then_some(before_nr), on_after.then_some(after_nr));

    let (mark, base, spans) = match dl {
        DiffLine::Unchanged(text) => {
            let syntax = walkers.and_then(|(before, after)| {
                before.skip();
                after.highlight_next()
            });
            let mut spans = vec![gutter.span(numbers.0, numbers.1, MARK_UNCHANGED, Style::new())];
            spans.extend(syntax_to_spans(syntax, text));
            return Line::from(spans);
        }
        DiffLine::Reindented { after, .. } => {
            let syntax = walkers.and_then(|(before, walker)| {
                before.skip();
                walker.highlight_next()
            });
            (
                MARK_REINDENTED,
                theme.diff_new,
                diff_change_spans(after, syntax, theme.diff_new, theme.diff_new_emphasis),
            )
        }
        DiffLine::Removed(ds) => {
            let syntax = walkers.and_then(|(before, _)| before.highlight_next());
            (
                MARK_REMOVED,
                theme.diff_old,
                diff_change_spans(ds, syntax, theme.diff_old, theme.diff_old_emphasis),
            )
        }
        DiffLine::Added(ds) => {
            let syntax = walkers.and_then(|(_, after)| after.highlight_next());
            (
                MARK_ADDED,
                theme.diff_new,
                diff_change_spans(ds, syntax, theme.diff_new, theme.diff_new_emphasis),
            )
        }
    };

    let mut row = vec![gutter.span(numbers.0, numbers.1, mark, base)];
    row.extend(spans);
    fill_row(&mut row, width, base);
    Line::from(row)
}

/// Runs a changed row's tint out to the edge of the card, so a diff reads as
/// bands of colour rather than as ragged highlights that stop wherever the code
/// happens to end. Unchanged rows are left alone, as they are in VS Code.
fn fill_row(spans: &mut Vec<Span<'static>>, width: u16, base: Style) {
    if width == UNCONSTRAINED_WIDTH {
        return;
    }
    let drawn: usize = spans.iter().map(|span| span.content.width()).sum();
    let Some(padding) = usize::from(width).checked_sub(drawn).filter(|pad| *pad > 0) else {
        return;
    };
    spans.push(Span::styled(" ".repeat(padding), base));
}

fn syntax_to_spans(syntax: Option<Vec<Span<'static>>>, text: &str) -> Vec<Span<'static>> {
    match syntax {
        Some(s) => s,
        None => vec![fallback_span(text)],
    }
}

fn diff_change_spans(
    ds: &[DiffSpan],
    syntax: Option<Vec<Span<'static>>>,
    base: Style,
    emph: Style,
) -> Vec<Span<'static>> {
    match syntax {
        Some(syn) => merge_syntax_with_diff(&syn, ds, base, emph),
        None => {
            let full: String = ds.iter().map(|s| s.text.as_str()).collect();
            vec![Span::styled(
                caudra_highlight::normalize_text(&full),
                theme::current().code_block.patch(base),
            )]
        }
    }
}

/// The `-a` and `+c` of a `@@ -a,b +c,d @@` header, which is where the hunk's
/// numbering restarts on each side. `None` for any line that is not a hunk
/// header, and for a header whose before side does not parse; a missing after
/// side falls back to the before one rather than losing the hunk.
fn hunk_start(line: &str) -> Option<(usize, usize)> {
    let mut fields = line.strip_prefix("@@ -")?.split(' ');
    let number = |field: Option<&str>| field?.split(',').next()?.parse().ok();
    let before: usize = number(fields.next())?;
    Some((
        before,
        number(fields.next().and_then(|f| f.strip_prefix('+'))).unwrap_or(before),
    ))
}

/// The side of the file a hunk's body line belongs to. A `\ No newline at end
/// of file` marker belongs to neither, and a line that lost its leading space
/// is context rather than a line whose first character is a prefix.
enum Side<'a> {
    Before(&'a str),
    After(&'a str),
    Both(&'a str),
    Neither,
}

fn side(raw: &str) -> Side<'_> {
    match raw.split_at_checked(1) {
        Some(("-", text)) => Side::Before(text),
        Some(("+", text)) => Side::After(text),
        Some(("\\", _)) => Side::Neither,
        Some((" ", text)) => Side::Both(text),
        _ => Side::Both(raw),
    }
}

/// One hunk of a unified patch: where its numbering restarts, and the body
/// lines it describes.
struct PatchHunk<'a> {
    before_start: usize,
    after_start: usize,
    body: Vec<&'a str>,
}

impl PatchHunk<'_> {
    /// The last line number the hunk prints on each side, which is what the
    /// gutter is sized against.
    fn ends(&self) -> (usize, usize) {
        let (before, after) = self
            .body
            .iter()
            .fold((0usize, 0usize), |(before, after), raw| match side(raw) {
                Side::Before(_) => (before + 1, after),
                Side::After(_) => (before, after + 1),
                Side::Both(_) => (before + 1, after + 1),
                Side::Neither => (before, after),
            });
        (
            self.before_start + before.saturating_sub(1),
            self.after_start + after.saturating_sub(1),
        )
    }

    /// The two file states the hunk describes, for the diff the card computes
    /// itself.
    fn sides(&self) -> (String, String) {
        let (mut before, mut after) = (String::new(), String::new());
        let push = |out: &mut String, text: &str| {
            out.push_str(text);
            out.push('\n');
        };
        for raw in &self.body {
            match side(raw) {
                Side::Before(text) => push(&mut before, text),
                Side::After(text) => push(&mut after, text),
                Side::Both(text) => {
                    push(&mut before, text);
                    push(&mut after, text);
                }
                Side::Neither => {}
            }
        }
        (before, after)
    }

    /// The hunk in the order the wire wrote it, for one too large to diff
    /// again.
    fn wire_lines(&self) -> Vec<DiffLine> {
        let change = |text: &str| {
            vec![DiffSpan {
                text: text.to_owned(),
                emphasized: false,
            }]
        };
        self.body
            .iter()
            .filter_map(|raw| match side(raw) {
                Side::Before(text) => Some(DiffLine::Removed(change(text))),
                Side::After(text) => Some(DiffLine::Added(change(text))),
                Side::Both(text) => Some(DiffLine::Unchanged(text.to_owned())),
                Side::Neither => None,
            })
            .collect()
    }
}

/// The `---`/`+++` preamble precedes the first header, so a line that starts
/// the same way inside a hunk is content.
fn patch_hunks(patch: &str) -> Vec<PatchHunk<'_>> {
    let mut hunks: Vec<PatchHunk> = Vec::new();
    for raw in patch.lines() {
        match hunk_start(raw) {
            Some((before_start, after_start)) => hunks.push(PatchHunk {
                before_start,
                after_start,
                body: Vec::new(),
            }),
            None => {
                if let Some(hunk) = hunks.last_mut() {
                    hunk.body.push(raw);
                }
            }
        }
    }
    hunks
}

/// A unified diff drawn the way an edit's diff is drawn: real line numbers
/// down the left, removed and added lines in the diff colours.
///
/// The wire's grouping is not trusted. A patch reports what was applied, and an
/// applied chunk arrives as the whole region it matched followed by the whole
/// region it produced, which prints every line they share twice. Diffing the
/// two sides again is what puts each change beside the line it replaces.
///
/// Syntax highlighting is left out on purpose, because a hunk carries only its
/// own context and a highlighter fed that much guesses wrong more than it
/// helps.
fn render_unified_patch(patch: &str, width: u16) -> Vec<Line<'static>> {
    let hunks = patch_hunks(patch);
    let (before_max, after_max) = hunks.iter().map(PatchHunk::ends).fold(
        (1, 1),
        |(before, after), (hunk_before, hunk_after)| {
            (before.max(hunk_before), after.max(hunk_after))
        },
    );
    let gutter = DiffGutter::new(before_max, after_max, width);
    let mut lines = Vec::new();
    for hunk in &hunks {
        let groups = if hunk.body.len() > MAX_REDIFF_LINES {
            vec![DiffHunk {
                before_start: 1,
                after_start: 1,
                lines: hunk.wire_lines(),
            }]
        } else {
            let (before, after) = hunk.sides();
            compute_hunks(&before, &after)
        };
        for group in groups {
            if !lines.is_empty() {
                lines.push(gap_ellipsis());
            }
            let mut cursor = (
                hunk.before_start + group.before_start - 1,
                hunk.after_start + group.after_start - 1,
            );
            for dl in &group.lines {
                lines.push(render_hunk_line(dl, None, &mut cursor, gutter, width));
            }
        }
    }
    lines
}

/// The live list lives in the bottom panel; this is the transcript copy, so a
/// reader scrolling back sees the plan as it stood at that point in the turn.
fn render_todos(items: &[TodoItem]) -> Vec<Line<'static>> {
    let t = theme::current();
    items
        .iter()
        .map(|item| {
            let style = match item.status {
                TodoStatus::Completed => t.todo_completed,
                TodoStatus::InProgress => t.todo_in_progress,
                TodoStatus::Pending => t.todo_pending,
                TodoStatus::Cancelled => t.todo_cancelled,
            };
            Line::from(Span::styled(
                format!("{} {}", item.status.marker(), item.content),
                style,
            ))
        })
        .collect()
}

/// The answers the user gave, one row per pick. Only the picks get a row:
/// every row here is permanent scrollback, and the options passed over are
/// spent information. The questions sit in the tool input right above this.
fn render_answers(answers: &[Answer]) -> Vec<Line<'static>> {
    let t = theme::current();
    let mut lines = Vec::new();
    for (index, answer) in answers.iter().enumerate() {
        let label = if answer.header.is_empty() {
            format!("Q{}", index + 1)
        } else {
            answer.header.clone()
        };
        lines.push(Line::styled(label, t.tool_prefix));
        if answer.labels.is_empty() {
            lines.push(Line::styled(
                format!("{ANSWER_INDENT}{NO_ANSWER}"),
                t.tool_dim,
            ));
            continue;
        }
        for picked in &answer.labels {
            for (row, piece) in picked.lines().enumerate() {
                let prefix = if row == 0 { ANSWER_MARK } else { ANSWER_INDENT };
                lines.push(Line::styled(format!("{prefix}{piece}"), t.todo_completed));
            }
        }
    }
    lines
}

/// A batch reads as a list of what it ran. Each child gets the one-line form
/// the transcript would show it with, then its own body indented under it, so
/// a child looks the same here as it does standalone: its sigil opens the row
/// and carries the outcome in its color, exactly as a compact row's does. A
/// status dot in front of that would only say a second time what the sigil's
/// color and the label's tense already say.
///
/// Spacing follows the transcript's own rule: a row that carries nothing but
/// itself is a list entry and stacks flush against its neighbours, while one
/// with a body has stopped being an entry and takes a blank row on both sides.
///
/// The transcript separates a row that merely wraps, which this cannot: these
/// are logical lines and the wrapping happens downstream, at a width no one
/// here knows. It reads as the list anyway, because every child opens on its
/// sigil and a continuation line does not, which is the distinction the blank
/// row was buying. Threading a width in would also put the gaps back exactly
/// where they are worst, since a long search header is what wraps.
fn render_batch(
    entries: &[BatchToolEntry],
    highlight: bool,
    limits: &RenderLimits,
) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>, Vec<ScrollSpan>) {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut spans_out = Vec::new();
    let mut previous_has_body = false;
    for (index, entry) in entries.iter().enumerate() {
        let view = limits.child(index, entry);
        // Resolved before the summary row so the separator below knows whether
        // this child is a list entry or a block.
        let body = view.map(|child| child_body(entry, highlight, &child, limits.live.get(&index)));
        let has_body = body.as_ref().is_some_and(|(lines, ..)| !lines.is_empty());
        // Every row of a child answers for it, whether or not a click would
        // change what is drawn: this is also how a dispatched child's rows are
        // traced back to the subagent they belong to.
        let target = Some(RowTarget(index));
        if !lines.is_empty() && (previous_has_body || has_body) {
            lines.push(Line::default());
            rows.push(None);
        }
        previous_has_body = has_body;
        let (sigil, label) = compact_sigil_label(&entry.tool, entry.status.into());
        // Once the body carries the script, the row keeps only what the body
        // does not say. The same trade a card's header makes, and for the same
        // reason: the body's copy is the numbered, highlighted one.
        let summary = match has_body && body_repeats_summary(entry) {
            true => "",
            false => entry.summary.as_str(),
        };
        let gap = if summary.is_empty() { "" } else { " " };
        let mut spans = vec![
            Span::styled(
                format!("{sigil} "),
                batch_sigil_style(entry.status, entry.output.as_ref()),
            ),
            Span::styled(format!("{label}{gap}"), t.tool_prefix),
        ];
        if !summary.is_empty() {
            spans.extend(header_spans(
                &entry.tool,
                summary,
                Style::default(),
                entry.raw_input.as_ref(),
            ));
        }
        if let Some(args) = compact_args_for(
            &entry.tool,
            &entry.summary,
            entry.raw_input.as_ref(),
            entry.output.as_ref(),
        ) {
            spans.push(Span::styled(args, t.tool_dim));
        }
        if let Some(annotation) = child_annotation(entry) {
            spans.push(Span::styled(format!(" ({annotation})"), t.tool_annotation));
        }
        if body.is_none() && holds_a_body(entry) {
            spans.push(Span::styled(BATCH_FOLDED_MARK, t.tool_dim));
        }
        lines.push(Line::from(spans));
        rows.push(target);
        // Between the header and the body, so a click below it still resolves
        // to the child it looks like it is on.
        if let Some(progress) = limits.progress.get(&index) {
            lines.push(Line::from(child_progress_spans(progress)));
            rows.push(target);
        }
        if let Some((body, _, span)) = body {
            if let Some(span) = span {
                spans_out.push(span.shifted(lines.len(), Some(index)));
            }
            rows.resize(rows.len() + body.len(), target);
            lines.extend(indent_all(body));
        }
    }
    (lines, rows, spans_out)
}

/// Whether opening this child would show anything, which is what the folded
/// mark promises. Answered from the output rather than by rendering one: a
/// folded child is redrawn as often as the card is, and the body it is hiding
/// may be long.
fn holds_a_body(entry: &BatchToolEntry) -> bool {
    entry
        .output
        .as_ref()
        .is_some_and(|output| !output.is_empty_result())
}

/// What a child row reports after its arguments. `batch` carries only the
/// annotation the dispatch returned, which is `None` for every tool that lets
/// its output speak instead, so a child lost the count its standalone row
/// shows: a grep of thirty files named no matches at all. A failure is left
/// out because its output is the error text, which the body already draws in
/// full and which reduces to a line count here.
///
/// A child that has not started says so. It reads the same as a failure
/// otherwise: both are drawn in the plain tense, so with nothing to separate
/// them a batch cut short looks like a batch that went wrong.
fn child_annotation(entry: &BatchToolEntry) -> Option<String> {
    if let Some(annotation) = &entry.annotation {
        return Some(annotation.clone());
    }
    match entry.status {
        BatchToolStatus::Pending => Some(QUEUED_ANNOTATION.to_owned()),
        BatchToolStatus::Success => entry.output.as_ref().and_then(ToolOutput::annotation),
        BatchToolStatus::Running | BatchToolStatus::Error => None,
    }
}

/// How a dispatched child is getting on. A settled child is described by its
/// output, so what it was doing gives way to what it did, and only the tally
/// survives as the record of work its output does not show.
fn child_progress_spans(progress: &ToolProgress) -> Vec<Span<'static>> {
    let theme = theme::current();
    let mut spans = vec![Span::styled(CHILD_ACTIVITY_PREFIX, theme.tool_dim)];
    if progress.is_live() {
        spans.push(Span::styled(
            progress.report.activity.label().to_owned(),
            theme.tool_prefix,
        ));
        if let Some(detail) = progress.report.activity.detail() {
            spans.push(Span::styled(format!(" {detail}"), theme.tool_dim));
        }
        spans.push(Span::styled(CHILD_ACTIVITY_SEPARATOR, theme.tool_dim));
    }
    spans.push(Span::styled(
        SubagentProgress::tally(progress.report.tools, progress.elapsed()),
        theme.tool_dim,
    ));
    spans
}

/// A child's own rendering, structured where the tool produced structure and
/// its text otherwise, with whether it is holding anything back. Errors read
/// as plain text: a failed call has no structured result to draw.
///
/// `live` is the tail a child that has not answered yet is streaming. A batch
/// clears its children's live sink, so the only thing a running child would
/// otherwise draw is the spinner on its summary row, and a long shell inside
/// a batch would show nothing at all until it finished.
fn child_body(
    entry: &BatchToolEntry,
    highlight: bool,
    limits: &RenderLimits,
    live: Option<&String>,
) -> (Vec<Line<'static>>, bool, Option<ScrollSpan>) {
    let output = entry.output.as_ref();
    // Every answer that is text goes through one place, so no arm can be the
    // one that forgets the script. `render_tool_content` draws the script
    // itself, which is why structured output is the exception here rather than
    // a case alongside the others.
    let text = if entry.status == BatchToolStatus::Error {
        Some(text_lines(
            output.map_or(String::new(), ToolOutput::as_text),
        ))
    } else if let Some(tail) = live.filter(|text| output.is_none() && !text.is_empty()) {
        Some(text_lines(tail.clone()))
    } else {
        match output {
            Some(ToolOutput::Markdown(text)) => Some(markdown_lines(&text.text, limits.width)),
            Some(ToolOutput::Plain(text) | ToolOutput::ReadDir(text)) => {
                Some(text_lines(text.text.clone()))
            }
            Some(ToolOutput::Shell(shell)) => Some(text_lines(shell.raw_text())),
            _ => None,
        }
    };
    match text {
        Some(lines) => {
            // A child that has answered has no tail left to chase, so its
            // footer is told to report where the window sits and nothing more.
            let tail = match entry.output.is_none() {
                true => ScrollTail::Live,
                false => ScrollTail::Settled,
            };
            with_script(entry, highlight, limits, child_view(lines, limits, tail))
        }
        None => {
            let content =
                render_tool_content(entry.input.as_ref(), output, highlight, limits.clone());
            (content.lines, content.truncation, None)
        }
    }
}

/// Whether an opened child's body prints its summary row again. A shell or
/// python call's summary is its script's first line, so the row and the script
/// say the same thing.
///
/// Compared rather than assumed from the tool name, because a `task` child's
/// summary is a description its body never repeats.
fn body_repeats_summary(entry: &BatchToolEntry) -> bool {
    matches!(
        entry.input.as_ref(),
        Some(ToolInput::Script { code, .. } | ToolInput::Code { code, .. })
            if code.lines().next() == Some(entry.summary.as_str())
    )
}

/// A child draws its script the way its own card does: numbered, highlighted,
/// and above what the command printed. `render_tool_content` renders structured
/// output, so a child answering with text took an arm that drew the output
/// alone and never asked for the script at all, which is why a shell child had
/// no highlighting while a read child did.
///
/// Added above the window rather than inside it, as a card does, so the script
/// stays in view while the output scrolls under it. A script inside the window
/// scrolls itself off, which is the one thing the reader asked to see.
///
/// Line count does not come into it. A one-line command gets the same numbered,
/// highlighted row a long one does, and `body_repeats_summary` then takes the
/// duplicate off the summary row.
fn with_script(
    entry: &BatchToolEntry,
    highlight: bool,
    limits: &RenderLimits,
    body: (Vec<Line<'static>>, bool, Option<ScrollSpan>),
) -> (Vec<Line<'static>>, bool, Option<ScrollSpan>) {
    let (output, truncation, span) = body;
    if entry.input.is_none() {
        return (output, truncation, span);
    }
    let mut lines =
        render_tool_content(entry.input.as_ref(), None, highlight, limits.clone()).lines;
    if !lines.is_empty() && !output.is_empty() {
        lines.push(Line::default());
    }
    let shift = lines.len();
    lines.extend(output);
    (lines, truncation, span.map(|span| span.shift_lines(shift)))
}

/// Holds a child to its window when it scrolls and to its budget otherwise.
///
/// The window takes the footer the budget's notice would have taken. Both say
/// what is not being shown; a window says it as two edges and which one the
/// reader is pinned to, because that is what tells them whether output is
/// still arriving under what they are reading.
fn child_view(
    lines: Vec<Line<'static>>,
    limits: &RenderLimits,
    tail: ScrollTail,
) -> (Vec<Line<'static>>, bool, Option<ScrollSpan>) {
    let Some(window) = limits.scroll else {
        let (lines, truncation) = capped(lines, limits.budget);
        return (lines, truncation, None);
    };
    let total = lines.len();
    let (start, end) = window.range(total);
    let mut shown = lines[start..end].to_vec();
    let span = ScrollSpan {
        child: None,
        first: 0,
        lines: shown.len(),
        total,
        offset: start,
    };
    let Some(footer) = scroll_footer_text(start, total - end, tail) else {
        return (shown, false, None);
    };
    shown.push(Line::from(Span::styled(footer, theme::current().tool_dim)));
    (shown, true, Some(span))
}

/// Holds a text body to the budget its own card would hold it to, and says how
/// much that hid. The tools that draw themselves already write this notice, so
/// a child announces what it is keeping back wherever the body came from.
fn capped(mut lines: Vec<Line<'static>>, budget: usize) -> (Vec<Line<'static>>, bool) {
    let hidden = lines.len().saturating_sub(budget);
    if !should_truncate(hidden) {
        return (lines, false);
    }
    lines.truncate(budget);
    lines.push(truncation_line(hidden));
    (lines, true)
}

/// A child that answered in markdown is answering, not quoting: a subagent's
/// report and a skill's instructions are prose, and showing them as source
/// puts the syntax on screen instead of what it says.
///
/// Given the columns the child actually has, which is what a code block or a
/// table needs to break itself. Left unbroken they run past the card and the
/// terminal wraps them, and `Wrap` restarts a continuation at column zero, so
/// the indent saying which child the row belongs to is lost. A subagent
/// reporting a structured result is a fenced block, so this is the case that
/// matters. Paragraphs are still ratatui's to wrap, here as everywhere else:
/// pre-breaking them would turn soft wraps into hard newlines in a copy.
///
/// The width rides on the limits, which is what reaches the highlight worker,
/// and `HighlightKey` carries it so a resize re-renders instead of splicing
/// back an answer broken for the width the terminal used to be. Zero is the
/// renderer's own word for not wrapping, and stays what a caller with no width
/// to give gets.
fn markdown_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let style = theme::current().assistant;
    let (painted, _) = text_to_painted(
        text,
        "",
        style,
        style,
        width,
        Some(caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES),
        Vec::new(),
    );
    painted.lines
}

fn text_lines(text: String) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| Line::from(line.to_owned()))
        .collect()
}

fn indent_all(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::raw(BATCH_CHILD_INDENT));
            line
        })
        .collect()
}

/// Each file gets its own heading, because a patch that touches three files
/// is otherwise three diffs with nothing saying where one ends.
fn render_patch(files: &[PatchedFile], width: u16) -> Vec<Line<'static>> {
    let theme = theme::current();
    let mut lines = Vec::new();
    for file in files {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::styled(file.path.clone(), theme.tool_prefix),
            Span::styled(
                format!(" +{} -{}", file.additions, file.deletions),
                theme.tool_annotation,
            ),
        ]));
        lines.extend(render_unified_patch(&file.patch, width));
    }
    lines
}

/// The file a write is still spelling out, drawn whole.
///
/// Deliberately unbounded: the finished card draws every line, so clipping
/// the live view would only make the card jump when the tool starts, and a
/// write's card is the file. What makes that affordable is that this runs
/// once per frame rather than once per fragment — see
/// `MessagesPanel::flush_live_bodies`. Rendering it per token cost the file's
/// length squared, which is what a long write used to feel like as lag.
///
/// `render_code` is told the window is the whole of what it is drawing, so it
/// reports nothing hidden: the rest of the file has not arrived yet, and
/// there is nothing a click could reveal.
///
/// Nothing here is highlighted. A file cut off mid-token leaves the parser in
/// a state the rest of the file has not justified yet, which is the same
/// reason a patch hunk is never highlighted either.
pub(crate) fn render_live_body(body: &str) -> Vec<Line<'static>> {
    let shown: Vec<String> = body.lines().map(String::from).collect();
    render_code(None, 1, &shown, shown.len(), usize::MAX).lines
}

/// How many rows the full rendering would take. Counted rather than rendered,
/// so deciding to condense never costs the highlighting of results nobody is
/// going to see.
fn grep_height(entries: &[GrepFileEntry], multi: bool) -> usize {
    entries
        .iter()
        .map(|entry| {
            let has_context = entry.groups.iter().any(|group| group.lines.len() > 1);
            let separators = if has_context {
                entry.groups.len().saturating_sub(1)
            } else {
                0
            };
            let lines: usize = entry.groups.iter().map(|group| group.lines.len()).sum();
            usize::from(multi) + separators + lines
        })
        .sum()
}

/// A notice costs the row it saves, so hiding exactly one of anything is
/// never worth it. Answers with how many to show and how many that hides.
fn within(total: usize, room: usize) -> (usize, usize) {
    let shown = total.min(room);
    if total - shown == 1 {
        (total, 0)
    } else {
        (shown, total - shown)
    }
}

/// What a reader wants first from a grep they cannot see all of is its shape:
/// how much matched and where. Match text answers a different question, and
/// is a click away. The totals are left to the card header, which already
/// annotates itself with them.
fn render_grep_summary(entries: &[GrepFileEntry], max_lines: usize) -> Vec<Line<'static>> {
    let theme = theme::current();
    let matches: usize = entries.iter().map(GrepFileEntry::match_count).sum();

    // The affordance keeps the last row, so the shape never crowds out the way
    // back to the detail.
    let (shown, hidden) = within(entries.len(), max_lines.saturating_sub(1));
    let mut out: Vec<Line<'static>> = entries
        .iter()
        .take(shown)
        .map(|entry| {
            Line::from(vec![
                Span::raw(GREP_SUMMARY_INDENT),
                Span::styled(entry.path.clone(), theme.tool_path),
                Span::styled(GREP_COUNT_SEP, theme.tool_dim),
                Span::styled(entry.match_count().to_string(), theme.tool_dim),
            ])
        })
        .collect();

    let gained = if hidden > 0 {
        format!("{hidden} files")
    } else {
        format!("{matches} matches")
    };
    out.push(Line::from(Span::styled(
        expand_notice(&gained),
        theme.tool_dim,
    )));
    out
}

fn render_grep_lines(
    entries: &[GrepFileEntry],
    mut budget: usize,
    highlight: bool,
    multi: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let global_max_nr = entries
        .iter()
        .flat_map(|e| {
            e.groups
                .iter()
                .flat_map(|g| g.lines.iter().map(|l| l.line_nr))
        })
        .max()
        .unwrap_or(1);
    let w = nr_width(global_max_nr);
    let dim = theme::current().tool_dim;

    for entry in entries {
        if budget == 0 {
            break;
        }

        if multi {
            out.push(Line::from(Span::styled(
                entry.path.clone(),
                theme::current().tool_path,
            )));
            budget -= 1;
        }

        let syntax = highlight.then(|| caudra_highlight::syntax_for_path(&entry.path));
        let has_context = entry.groups.iter().any(|g| g.lines.len() > 1);

        for (gi, group) in entry.groups.iter().enumerate() {
            if budget == 0 {
                break;
            }
            if gi > 0 && has_context {
                out.push(Line::from(Span::styled("  --".to_owned(), dim)));
                budget -= 1;
            }
            for line in &group.lines {
                if budget == 0 {
                    break;
                }
                let mut spans = vec![gutter(&format!("{:>w$}", line.line_nr))];
                let text_spans = if let Some(syn) = syntax {
                    highlight_spans(
                        &mut caudra_highlight::Highlighter::for_syntax(syn),
                        &line.text,
                    )
                } else if line.is_match {
                    vec![fallback_span(&line.text)]
                } else {
                    vec![Span::styled(line.text.clone(), dim)]
                };
                if line.is_match {
                    spans.extend(text_spans);
                } else {
                    spans.extend(
                        text_spans
                            .into_iter()
                            .map(|s| Span::styled(s.content, theme::dim_style(s.style, 0.3))),
                    );
                }
                out.push(Line::from(spans));
                budget -= 1;
            }
        }
    }
    out
}

fn render_grep_results(
    entries: &[GrepFileEntry],
    capped: Option<&SearchCap>,
    max_lines: usize,
    highlight: bool,
) -> (Vec<Line<'static>>, bool) {
    let (mut out, truncated) = render_grep_matches(entries, max_lines, highlight);
    // A capped search read part of the tree, so an absent match is not evidence
    // that there is none. Saying how far it got is what separates the two.
    if let Some(cap) = capped {
        out.push(Line::from(Span::styled(
            format!(
                "searched {} of {} files; more matches may exist",
                cap.files_scanned, cap.files_listed
            ),
            theme::current().tool_dim,
        )));
    }
    (out, truncated)
}

fn render_grep_matches(
    entries: &[GrepFileEntry],
    max_lines: usize,
    highlight: bool,
) -> (Vec<Line<'static>>, bool) {
    let multi = entries.len() > 1;
    let height = grep_height(entries, multi);
    if height <= max_lines {
        return (render_grep_lines(entries, height, highlight, multi), false);
    }
    if multi {
        return (render_grep_summary(entries, max_lines), true);
    }

    // One file has no distribution to summarise, so it names itself and the
    // matches take what room is left. The count is left to the card header.
    let theme = theme::current();
    let path = entries.first().map(|e| e.path.clone()).unwrap_or_default();
    let mut out = vec![Line::from(Span::styled(path, theme.tool_path))];
    let (shown, hidden) = within(height, max_lines.saturating_sub(2));
    out.extend(render_grep_lines(entries, shown, highlight, false));
    out.push(Line::from(Span::styled(
        expand_notice(&format!("{hidden} lines")),
        theme.tool_dim,
    )));
    (out, true)
}

fn index_range(range: IndexSourceRange) -> String {
    if range.start_line == range.end_line {
        format!("[{}]", range.start_line)
    } else {
        format!("[{}-{}]", range.start_line, range.end_line)
    }
}

fn index_highlight_token(language: &str) -> &str {
    match language {
        "c_sharp" => "cs",
        "lua_lang" => "lua",
        "bazel_build" | "bazel_module" | "bazel_bzl" => "bzl",
        "containerfile" => "dockerfile",
        "make" => "Makefile",
        _ => language,
    }
}

fn render_index_file(
    language: &str,
    index_lines: &[IndexLine],
    max_lines: usize,
    highlight: bool,
) -> (Vec<Line<'static>>, bool) {
    let capped = index_lines.len().min(max_lines);
    let hidden = index_lines.len().saturating_sub(capped);
    let truncated = should_truncate(hidden);
    let display_count = if truncated { capped } else { index_lines.len() };
    let mut lines = Vec::with_capacity(display_count + usize::from(truncated));
    for line in index_lines.iter().take(display_count) {
        let body = line.body.as_deref().unwrap_or(&line.text);
        let mut spans = match line.semantic {
            IndexLineSemantic::Section => {
                vec![Span::styled(
                    body.to_owned(),
                    theme::current().index_section,
                )]
            }
            IndexLineSemantic::Dimmed => {
                vec![Span::styled(line.text.clone(), theme::current().tool_dim)]
            }
            IndexLineSemantic::Item | IndexLineSemantic::Plain if highlight => highlight_spans(
                &mut caudra_highlight::Highlighter::for_token(index_highlight_token(language)),
                body,
            ),
            IndexLineSemantic::Item | IndexLineSemantic::Plain => {
                vec![Span::styled(body.to_owned(), theme::current().tool)]
            }
        };
        if let Some(range) = line.source_range {
            if !body.ends_with(' ') {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                index_range(range),
                theme::current().index_line_nr,
            ));
        }
        lines.push(Line::from(spans));
    }
    if truncated {
        lines.push(truncation_line(hidden));
    }
    (lines, truncated)
}

/// Caveats the graph reports about itself. A rank caught mid-descent and a
/// crawl that stopped early both order the same way a complete one does, so the
/// footer is the only place a reader learns the difference.
const GRAPH_CAVEATS: &[&str] = &["NOT converged", "truncated by", "stopped early"];
const GRAPH_ROW_SIGIL: &str = "\u{25c7} ";

/// One symbol per line, with the name leading and everything measured about it
/// trailing in dimmed columns.
///
/// The name column is padded to the widest name actually shown rather than a
/// fixed width, so a narrow result does not sit in a gutter of blanks.
fn render_code_graph(
    headline: &str,
    rows: &[CodeGraphRow],
    source: Option<&CodeGraphSource>,
    footer: &str,
    max_lines: usize,
    highlight: bool,
) -> (Vec<Line<'static>>, bool) {
    let theme = theme::current();
    // The headline and footer are the two lines that make the rest
    // interpretable, so they are never what gets dropped.
    let room = max_lines.saturating_sub(2);
    let body_height = rows.len() + source.map_or(0, |source| source.lines.len() + 1);
    let (shown, hidden) = within(body_height, room);
    let truncated = hidden > 0;

    let mut lines = vec![Line::from(Span::styled(
        headline.to_owned(),
        theme.tool_dim,
    ))];

    let row_count = rows.len().min(shown);
    let name_width = rows
        .iter()
        .take(row_count)
        .map(|row| row.name.chars().count())
        .max()
        .unwrap_or_default();
    let hop_width = rows
        .iter()
        .take(row_count)
        .filter_map(|row| row.hops)
        .max()
        .map(|hops| hops.to_string().len());
    for row in rows.iter().take(row_count) {
        lines.push(code_graph_row(row, name_width, hop_width));
    }

    if let Some(source) = source {
        let body_room = shown.saturating_sub(row_count);
        if body_room > 0 {
            lines.push(Line::from(Span::styled(
                match &source.whole_file_reason {
                    Some(reason) => format!("{} \u{2014} {reason}", source.path),
                    None => source.path.clone(),
                },
                theme.tool_path,
            )));
            let body = render_code(
                highlight.then(|| caudra_highlight::Highlighter::for_path(&source.path)),
                source.line_start,
                &source.lines,
                source.lines.len(),
                body_room.saturating_sub(1),
            );
            lines.extend(body.lines);
        }
    }

    if truncated {
        lines.push(truncation_line(hidden));
    }
    let caveated = GRAPH_CAVEATS.iter().any(|caveat| footer.contains(caveat));
    lines.push(Line::from(Span::styled(
        footer.to_owned(),
        if caveated {
            theme.error
        } else {
            theme.tool_dim
        },
    )));
    (lines, truncated)
}

fn code_graph_row(
    row: &CodeGraphRow,
    name_width: usize,
    hop_width: Option<usize>,
) -> Line<'static> {
    let theme = theme::current();
    let mut spans = vec![Span::styled(GRAPH_ROW_SIGIL.to_owned(), theme.tool_dim)];
    if let Some(width) = hop_width {
        spans.push(Span::styled(
            match row.hops {
                Some(hops) => format!("{hops:>width$} "),
                None => " ".repeat(width + 1),
            },
            theme.index_line_nr,
        ));
    }
    spans.push(Span::styled(
        format!("{:<name_width$}", row.name),
        theme.tool,
    ));
    spans.push(Span::styled(format!("  {}", row.kind), theme.tool_dim));
    spans.push(Span::styled(
        format!("  {}:{}-{}", row.path, row.line_start, row.line_end),
        theme.tool_path,
    ));
    if let (Some(inbound), Some(outbound)) = (row.inbound, row.outbound) {
        spans.push(Span::styled(
            format!("  in {inbound:>3} out {outbound:>3}"),
            theme.tool_dim,
        ));
    }
    if row.test_scope {
        spans.push(Span::styled("  [test]".to_owned(), theme.index_section));
    }
    Line::from(spans)
}

fn render_index_directory(output: &IndexOutput, max_lines: usize) -> (Vec<Line<'static>>, bool) {
    let IndexOutput::Directory {
        entries,
        listing,
        truncated: source_truncated,
        ..
    } = output
    else {
        return (Vec::new(), false);
    };
    let mut listing_lines = listing.lines().collect::<Vec<_>>();
    if *source_truncated && listing_lines.last() == Some(&INDEX_TRUNCATED) {
        listing_lines.pop();
    }
    let capped = listing_lines.len().min(max_lines);
    let hidden = listing_lines.len().saturating_sub(capped);
    let truncated = should_truncate(hidden);
    let display_count = if truncated {
        capped
    } else {
        listing_lines.len()
    };
    let mut lines = listing_lines
        .into_iter()
        .take(display_count)
        .enumerate()
        .map(|(index, text)| {
            let style = match entries.get(index).map(|entry| entry.kind) {
                Some(IndexDirectoryEntryKind::Directory) => theme::current().tool_path,
                Some(IndexDirectoryEntryKind::File) => theme::current().tool,
                None => theme::current().tool_dim,
            };
            Line::from(Span::styled(text.to_owned(), style))
        })
        .collect::<Vec<_>>();
    if truncated {
        lines.push(truncation_line(hidden));
    }
    if *source_truncated {
        lines.push(Line::from(Span::styled(
            INDEX_TRUNCATED,
            theme::current().tool_dim,
        )));
    }
    (lines, truncated)
}

pub(crate) fn render_instructions(
    blocks: &[InstructionBlock],
    lines: &mut Vec<Line<'static>>,
    max_lines: usize,
    highlight: bool,
) -> bool {
    let dim = theme::current().tool_dim;
    let mut used = 0;
    let mut truncated = false;
    let multi = blocks.len() > 1;

    for (i, block) in blocks.iter().enumerate() {
        if used >= max_lines {
            truncated = true;
            break;
        }

        if multi {
            lines.push(Line::from(Span::styled(block.path.clone(), dim)));
            used += 1;
            if i > 0 && used >= max_lines {
                truncated = true;
                break;
            }
        }

        if block.content.is_empty() {
            continue;
        }

        let code_lines: Vec<String> = block.content.lines().map(String::from).collect();
        let total = code_lines.len();
        let remaining = max_lines.saturating_sub(used);
        let hl = highlight.then(|| caudra_highlight::Highlighter::for_path(&block.path));
        let rendered = render_code(hl, 1, &code_lines, total, remaining);
        used += rendered.lines.len();
        truncated |= rendered.truncated;
        lines.extend(rendered.lines);
    }
    truncated
}

/// What the reader has asked of one card. Held as `Option`: `None` draws the
/// header alone, `full: false` draws the body within the tool's row budget,
/// and `full: true` draws all of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Disclosure {
    /// The whole body rather than the tool's row budget.
    pub full: bool,
    /// Shell only: the raw capture instead of the filtered text.
    pub shell_raw: bool,
}

/// The batch children the reader has asked to see whole, by their index in the
/// roster. A child not named here is drawn the way its own card would be: a
/// read-only one folds to its summary row, so a batch nobody has touched reads
/// as the list of what it ran, and one whose body is the only record of what it
/// did rests at its tool's budget.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct BatchViews(Arc<[usize]>);

impl BatchViews {
    pub fn new(open: impl IntoIterator<Item = usize>) -> Self {
        let mut open: Vec<usize> = open.into_iter().collect();
        open.sort_unstable();
        open.dedup();
        Self(open.into())
    }

    fn is_open(&self, index: usize) -> bool {
        self.0.contains(&index)
    }

    /// Every control undoes itself: the click that opened a child folds it
    /// again.
    pub fn toggled(&self, index: usize) -> Self {
        Self::new(
            self.0
                .iter()
                .copied()
                .filter(|held| *held != index)
                .chain((!self.is_open(index)).then_some(index)),
        )
    }
}

/// What each dispatched child of one batch is doing, by index in the roster.
/// Shared rather than cloned: a running batch rebuilds at the spinner cadence
/// and the reports outlive none of it.
pub type ChildProgress = Arc<HashMap<usize, ToolProgress>>;

/// The live tail of each child of one batch that has not answered yet, by
/// index in the roster.
pub type ChildLive = Arc<HashMap<usize, String>>;

/// A fixed-height window onto a body that may be longer than it. `follow`
/// pins the window to the tail, so a body still arriving keeps its newest
/// lines on screen; scrolling up drops the pin and `offset` takes over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollWindow {
    pub height: usize,
    pub offset: usize,
    pub follow: bool,
}

impl ScrollWindow {
    /// The half-open range of `total` lines this window shows. Following
    /// takes the tail; a paused window is clamped so a body that shrank
    /// cannot leave it pointing past the end.
    pub fn range(self, total: usize) -> (usize, usize) {
        let height = self.height.min(total);
        let max_start = total - height;
        let start = if self.follow {
            max_start
        } else {
            self.offset.min(max_start)
        };
        (start, start + height)
    }
}

/// The tools whose body reports on work rather than being the work, and is
/// long, arrives over time, or both. They are drawn in a fixed window that
/// follows the tail rather than abridged with a notice offering the rest.
///
/// A write is deliberately absent. Its body is the file it wrote, so a window
/// on the tail hides the head of what it just did, which is the part worth
/// reading.
const SCROLL_CARD_TOOLS: &[&str] = &[SHELL_TOOL_NAME, PYTHON_EXECUTION_TOOL_NAME, TASK_TOOL_NAME];

/// What the reader's configuration and view mode say about a tool. Carried
/// with the limits rather than looked up at the card, because a batch child is
/// the same call without a card of its own and has to answer these the same
/// way or the two drift.
#[derive(Clone, Default)]
pub struct CardPolicy {
    pub always_collapsed: Arc<[String]>,
    pub scroll_card_lines: u32,
    /// Whether the reader asked for one row per call. Held here so a child
    /// folds by the same answer its own card would give.
    pub compact: bool,
}

impl CardPolicy {
    /// Whether no view mode opens this tool's body.
    pub fn stays_collapsed(&self, tool: &str) -> bool {
        self.always_collapsed
            .iter()
            .any(|pattern| names_tool(pattern, tool))
    }

    /// Whether this tool's body is drawn in a fixed window rather than
    /// abridged to a budget.
    pub fn scrolls(&self, tool: &str) -> bool {
        self.scroll_card_lines > 0
            && SCROLL_CARD_TOOLS
                .iter()
                .any(|known| names_tool(known, tool))
    }

    pub fn window(&self, tool: &str, offset: usize, follow: bool) -> Option<ScrollWindow> {
        self.scrolls(tool).then_some(ScrollWindow {
            height: self.scroll_card_lines as usize,
            offset,
            follow,
        })
    }
}

#[derive(Clone, Default)]
pub struct RenderLimits {
    pub budget: usize,
    /// Present only for a card drawn as a fixed-height scroller. It replaces
    /// the budget and the notice under it: the window is the whole story of
    /// how much is shown, and the footer says where it sits.
    pub scroll: Option<ScrollWindow>,
    pub policy: CardPolicy,
    /// Where each scrolling child of this card has its window, by index in
    /// the roster. A child absent from the map is pinned to its tail.
    pub child_scroll: Arc<HashMap<usize, ScrollWindow>>,
    pub views: BatchViews,
    pub progress: ChildProgress,
    /// The tail each child that has not answered yet is streaming, by index
    /// in the roster.
    pub live: ChildLive,
    /// Every tool's budget rather than only this card's, because a batch child
    /// rests at the one its own tool would be drawn with.
    pub tool_lines: ToolOutputLines,
    /// The columns the body has, already net of the indent its lines are
    /// prefixed with. Zero means the renderer should not wrap, which is what a
    /// caller with no width to give gets.
    pub width: u16,
}

impl RenderLimits {
    pub fn new(full: bool, budget: usize, views: BatchViews, tool_lines: ToolOutputLines) -> Self {
        Self {
            budget: if full { usize::MAX } else { budget },
            scroll: None,
            policy: CardPolicy::default(),
            child_scroll: Arc::default(),
            views,
            progress: ChildProgress::default(),
            live: ChildLive::default(),
            tool_lines,
            width: UNCONSTRAINED_WIDTH,
        }
    }

    pub fn with_scroll(self, scroll: Option<ScrollWindow>) -> Self {
        Self { scroll, ..self }
    }

    pub fn with_policy(
        self,
        policy: CardPolicy,
        child_scroll: Arc<HashMap<usize, ScrollWindow>>,
    ) -> Self {
        Self {
            policy,
            child_scroll,
            ..self
        }
    }

    pub fn with_progress(self, progress: ChildProgress, live: ChildLive) -> Self {
        Self {
            progress,
            live,
            ..self
        }
    }

    pub fn with_width(self, width: u16) -> Self {
        Self { width, ..self }
    }

    /// Whether any child is still moving. A report is redrawn every tick from
    /// a clock the highlight worker does not have, and a stream arrives as
    /// often as the command prints, so a batch holding either renders here
    /// instead of being sent out and spliced back stale.
    ///
    /// Neither is in the worker's cache key, and neither usefully could be:
    /// both move on every frame. Without this a streaming child shows the
    /// first window that reached the worker and then freezes there until the
    /// call settles.
    pub fn has_live_rows(&self) -> bool {
        !self.live.is_empty() || self.progress.values().any(ToolProgress::is_live)
    }

    pub fn is_expanded(&self) -> bool {
        self.budget == usize::MAX
    }

    /// How much of one child to draw, or `None` to fold it to its summary row.
    ///
    /// A child the reader asked for draws whole. Otherwise it answers the
    /// question its own card answers: a call whose body is the only record of
    /// what it did is drawn, resting at the budget that card would rest at,
    /// and one the header already accounts for folds away. Opening the parent
    /// says nothing about any of it: the card's own body is the list of what
    /// ran, and each child answers for itself.
    ///
    /// A tool the reader put on the always-collapsed list folds here too:
    /// the same call folds whether it was dispatched on its own or inside a
    /// batch, and a child is the one place the reader cannot reach a view
    /// mode to say otherwise.
    ///
    /// A scrolling child keeps its window whether the reader opened it or
    /// not. The window is what bounds the row in the first place, so opening
    /// one cannot be allowed to spill a whole shell log into the list.
    ///
    /// A child still streaming is drawn whatever its settled row would fold
    /// to. A live body is the reason to be watching the call at all, and a
    /// shell folds once it answers, so without this the one moment its output
    /// is worth showing is the one moment it is hidden. The reader's
    /// never-open list still wins: that list is about output nobody wants,
    /// whether it has arrived yet or not.
    ///
    /// Compact overrides every one of those. The reader asked for one row per
    /// call, and a batch is only open there because its list is the whole of
    /// what it has to say, so a child that filled the list with its own body
    /// would take back what the mode was chosen for. Opening a child by hand
    /// is still the way out, and it opens whole.
    ///
    /// The views and the reports name this card's children, so both are
    /// dropped on the way in or a nested batch would read them as its own.
    fn child(&self, index: usize, entry: &BatchToolEntry) -> Option<Self> {
        let open = self.views.is_open(index);
        if self.policy.compact && !open {
            return None;
        }
        let streaming = entry.output.is_none()
            && self.live.get(&index).is_some_and(|tail| !tail.is_empty())
            && !self.policy.stays_collapsed(&entry.tool);
        let scroll = self.child_scroll.get(&index).copied().or_else(|| {
            self.policy
                .window(&entry.tool, 0, true)
                .filter(|_| open || streaming || !is_collapsible(entry.effect, &entry.tool))
        });
        let budget = match scroll {
            Some(window) => window.height,
            None if open => usize::MAX,
            None if self.policy.stays_collapsed(&entry.tool) => return None,
            None if streaming => self.tool_lines.get(&entry.tool),
            None if is_collapsible(entry.effect, &entry.tool) => return None,
            None => self.tool_lines.get(&entry.tool),
        };
        Some(Self {
            budget,
            scroll,
            policy: self.policy.clone(),
            child_scroll: Arc::default(),
            views: BatchViews::default(),
            progress: ChildProgress::default(),
            live: ChildLive::default(),
            tool_lines: self.tool_lines,
            width: self.width.saturating_sub(BATCH_CHILD_INDENT_WIDTH),
        })
    }
}

/// The child views of every card that has any, by parent tool id.
pub type BatchViewMap = HashMap<String, BatchViews>;

/// The child reports of every batch that has any, by parent tool id.
pub type BatchProgressMap = HashMap<String, ChildProgress>;

/// The live output of every batch that has a child still streaming, by parent
/// tool id.
pub type BatchLiveMap = HashMap<String, ChildLive>;

/// What a body line belongs to, so a click can name a row after the async
/// highlight has replaced the spans under it: a batch child by its roster
/// index, or the scratch file line of a workflow card. A child is folded or
/// whole, so its summary row and its body are one control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowTarget(pub usize);

pub struct ToolContent {
    pub lines: Vec<Line<'static>>,
    /// Parallel to `lines`. Both render paths build it the same way, so the
    /// highlighted lines carry the same rows as the ones they replace.
    pub rows: Vec<Option<RowTarget>>,
    pub truncation: bool,
    pub scroll_spans: Vec<ScrollSpan>,
    /// Where every painted line came from, or `None` when a renderer in the
    /// body names no source and copy has to scrape the screen instead. Built
    /// by the same pass that built `lines`, because the highlighted render
    /// changes the span count of every code row.
    pub source: Option<BodySource>,
}

/// Where one window sits in a body, and how far into that body it sits, so a
/// bar can be placed beside it and dragged.
///
/// Recorded while the body is built, because that is the only point at which
/// the window and the lines it selected are both in hand. A bar derived any
/// later would have to guess which rows the window took, and a guess that
/// disagrees with the paint by one row is a bar that scrolls the wrong thing.
#[derive(Clone, Copy)]
pub struct ScrollSpan {
    /// The batch child this window belongs to, `None` for the card's own body.
    pub child: Option<usize>,
    /// Where the window starts among the lines of the segment it is drawn in.
    pub first: usize,
    /// Lines of window, which is what the bar's track spans.
    pub lines: usize,
    /// Lines of body the window is a view onto, and how far down it sits.
    pub total: usize,
    pub offset: usize,
}

impl ScrollSpan {
    /// Shifts a span recorded against a body into the segment that body was
    /// appended to. A child's window is built without knowing where in the
    /// card its rows will land, and the card's own header sits above it.
    fn shifted(self, by: usize, child: Option<usize>) -> Self {
        Self {
            child,
            first: self.first + by,
            ..self
        }
    }

    /// The same shift with the owner left alone, for a card appending a body
    /// whose children have already been named.
    pub fn shift_lines(self, by: usize) -> Self {
        self.shifted(by, self.child)
    }
}

pub fn render_tool_content(
    input: Option<&ToolInput>,
    output: Option<&ToolOutput>,
    highlight: bool,
    limits: RenderLimits,
) -> ToolContent {
    let mut lines = Vec::new();
    let mut truncation = false;
    let mut output_rows: Vec<Option<RowTarget>> = Vec::new();
    let mut output_spans: Vec<ScrollSpan> = Vec::new();
    let mut trace = SourceTrace::default();
    let mut output_source: Option<BodySource> = None;
    if let Some((language, code)) = input.map(|i| match i {
        ToolInput::Script { language, code } | ToolInput::Code { language, code } => {
            (language, code)
        }
    }) {
        let code_lines: Vec<String> = code
            .trim_end_matches('\n')
            .lines()
            .map(String::from)
            .collect();
        let total = code_lines.len();
        let hl = highlight.then(|| caudra_highlight::Highlighter::for_token(language));
        // A card that draws its script at all draws the whole of it. The
        // script is the record of what ran, it cannot be reconstructed from
        // the output, and it is bounded by what the model wrote. The budget
        // belongs to the output, which the tool can make arbitrarily long.
        let script = render_code(hl, 1, &code_lines, total, total);
        trace.record(lines.len(), script.source.named(language));
        lines.extend(script.lines);
    }
    let (output_lines, output_trunc) = match output {
        Some(ToolOutput::ReadCode {
            path,
            start_line,
            lines: code_lines,
            ..
        }) => {
            let code = render_code(
                highlight.then(|| caudra_highlight::Highlighter::for_path(path)),
                *start_line,
                code_lines,
                code_lines.len(),
                limits.budget,
            );
            output_source = Some(code.source.named_for_path(path));
            (code.lines, code.truncated)
        }
        Some(ToolOutput::WriteCode {
            path,
            lines: code_lines,
            ..
        }) => {
            let code = render_code(
                highlight.then(|| caudra_highlight::Highlighter::for_path(path)),
                1,
                code_lines,
                code_lines.len(),
                limits.budget,
            );
            output_source = Some(code.source.named_for_path(path));
            (code.lines, code.truncated)
        }
        Some(ToolOutput::Diff {
            path,
            before,
            after,
            ..
        }) => (
            render_diff(
                highlight.then(|| caudra_highlight::syntax_for_path(path)),
                before,
                after,
                limits.width,
            ),
            false,
        ),
        Some(ToolOutput::Patch { files }) => (render_patch(files, limits.width), false),
        Some(ToolOutput::GrepResult { entries, capped }) => {
            render_grep_results(entries, capped.as_ref(), limits.budget, highlight)
        }
        Some(ToolOutput::Index(IndexOutput::File {
            language, lines, ..
        })) => render_index_file(language, lines, limits.budget, highlight),
        Some(ToolOutput::CodeGraph {
            headline,
            rows,
            source,
            footer,
            ..
        }) => render_code_graph(
            headline,
            rows,
            source.as_ref(),
            footer,
            limits.budget,
            highlight,
        ),
        Some(ToolOutput::Index(output @ IndexOutput::Directory { .. })) => {
            render_index_directory(output, limits.budget)
        }
        Some(ToolOutput::Instructions { blocks }) => {
            let mut instruction_lines = Vec::new();
            let trunc =
                render_instructions(blocks, &mut instruction_lines, limits.budget, highlight);
            (instruction_lines, trunc)
        }
        Some(ToolOutput::TodoList(items)) => (render_todos(items), false),
        Some(ToolOutput::Answers(answers)) => (render_answers(answers), false),
        Some(ToolOutput::WorkflowRun(card)) => {
            let (card_lines, rows) = workflow_card::render(card);
            output_rows = rows;
            (card_lines, false)
        }
        // Each child owns how much of itself it shows, so the card reports no
        // truncation of its own: there is no one thing for it to open.
        Some(ToolOutput::Batch { entries, .. }) if !entries.is_empty() => {
            let (batch_lines, rows, spans) = render_batch(entries, highlight, &limits);
            output_rows = rows;
            output_spans = spans;
            (batch_lines, false)
        }
        Some(ToolOutput::ReadDir(_)) => (Vec::new(), false),
        _ => (Vec::new(), false),
    };
    truncation |= output_trunc;
    if !lines.is_empty() && !output_lines.is_empty() {
        lines.push(Line::default());
    }
    let mut rows = vec![None; lines.len()];
    rows.resize(lines.len() + output_lines.len(), None);
    for (row, target) in rows.iter_mut().skip(lines.len()).zip(output_rows) {
        *row = target;
    }
    let body_start = lines.len();
    match output_source {
        Some(source) => trace.record(body_start, source),
        // Rows a renderer drew without naming their source cannot be sliced,
        // and copying them as nothing is worse than scraping them back.
        None if !output_lines.is_empty() => trace.abandon(),
        None => {}
    }
    lines.extend(output_lines);
    let source = trace.finish(&lines);
    ToolContent {
        lines,
        rows,
        truncation,
        scroll_spans: output_spans
            .into_iter()
            .map(|span| span.shifted(body_start, span.child))
            .collect(),
        source,
    }
}

fn merge_syntax_with_diff(
    syntax_spans: &[Span<'static>],
    diff_spans: &[DiffSpan],
    base: Style,
    emphasis: Style,
) -> Vec<Span<'static>> {
    let mut result = Vec::new();

    let syn_iter = syntax_spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)));

    let mut diff_iter = diff_spans.iter().flat_map(|ds| {
        let bg = if ds.emphasized { emphasis } else { base };
        ds.text.chars().map(move |_| bg)
    });

    let mut current_text = String::new();
    let mut current_style: Option<Style> = None;

    for (syn_char, syn_style) in syn_iter {
        let bg = diff_iter.next().unwrap_or(base);
        let combined = syn_style.patch(bg);

        if current_style == Some(combined) {
            current_text.push(syn_char);
        } else {
            if !current_text.is_empty() {
                result.push(Span::styled(
                    std::mem::take(&mut current_text),
                    current_style.unwrap(),
                ));
            }
            current_text.push(syn_char);
            current_style = Some(combined);
        }
    }

    if !current_text.is_empty() {
        result.push(Span::styled(current_text, current_style.unwrap()));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{EXPAND_AFFORDANCE, TRUNCATION_PREFIX};
    use caudra_agent::GrepMatchGroup;
    use caudra_agent::tools::ToolEffect;
    use test_case::test_case;

    fn plain(text: &str) -> DiffSpan {
        DiffSpan {
            text: text.into(),
            emphasized: false,
        }
    }

    use ratatui::style::Color;

    const READ_MAX_LINES: usize = 5;

    #[test_case(20, 20, READ_MAX_LINES + 1 ; "truncates_with_ellipsis")]
    #[test_case(3,  3,  3                    ; "no_truncation_when_short")]
    #[test_case(5,  50, 5 + 1                ; "total_exceeds_available_lines")]
    #[test_case(6,  6,  6                    ; "one_hidden_shows_all")]
    fn render_code_line_count(input_lines: usize, total: usize, expected: usize) {
        let code_lines: Vec<String> = (0..input_lines).map(|i| format!("line {i}")).collect();
        let result = render_code(
            Some(caudra_highlight::Highlighter::for_path("test.rs")),
            1,
            &code_lines,
            total,
            READ_MAX_LINES,
        );
        assert_eq!(result.lines.len(), expected);
        assert_eq!(result.source.rows.len(), expected, "{ROWS_PER_LINE}");
    }

    const ROWS_PER_LINE: &str =
        "provenance needs one row per painted line or extraction gives up on the card";

    /// The gutter is what a copy must not pick up, and the only thing standing
    /// between the clipboard and it is the first span being marked as chrome.
    #[test_case(false ; "plain")]
    #[test_case(true ; "highlighted")]
    fn a_code_row_names_its_source_behind_a_chrome_gutter(highlight: bool) {
        const CODE: &str = "fn main() { let x = 1; }";
        let code_lines = vec![CODE.to_owned()];
        let hl = highlight.then(|| caudra_highlight::Highlighter::for_path("test.rs"));

        let rendered = render_code(hl, 1, &code_lines, 1, usize::MAX);

        assert_eq!(rendered.source.text, CODE);
        let row = &rendered.source.rows[0];
        assert_eq!(row.line, Some(0..CODE.len() as u32));
        assert_eq!(
            row.spans.len(),
            rendered.lines[0].spans.len(),
            "{SPANS_PER_ROW}"
        );
        assert_eq!(row.spans[0], SpanSource::Chrome, "{GUTTER_IS_CHROME}");
    }

    const SPANS_PER_ROW: &str = "a row's sources must stay parallel to its painted spans";
    const GUTTER_IS_CHROME: &str = "the line-number gutter must name no source";

    const PATCH: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -8,3 +8,4 @@\n context\n-gone\n+added\n+also added\n";
    const NUMBERED_MSG: &str = "context and removed lines carry their real file line number";
    const AFTER_GUTTER_MSG: &str = "an added line is numbered from the side it exists on";
    const HUNK_GAP_MSG: &str = "a jump between hunks must be marked, not silently closed";
    const HEADING_MSG: &str = "each file names itself and its size";

    fn patch_text(files: &[PatchedFile]) -> Vec<String> {
        render_patch(files, UNCONSTRAINED_WIDTH)
            .iter()
            .map(line_text)
            .collect()
    }

    fn one_file(patch: &str) -> Vec<PatchedFile> {
        vec![PatchedFile {
            path: "src/lib.rs".into(),
            patch: patch.into(),
            additions: 2,
            deletions: 1,
        }]
    }

    /// Numbering restarts at each `@@` header, so a hunk deep in a file reads
    /// against the file rather than against the patch.
    #[test]
    fn a_patch_numbers_its_lines_from_the_hunk_header() {
        let rendered = patch_text(&one_file(PATCH));
        assert!(
            rendered.contains(&" 8   context".to_owned()),
            "{NUMBERED_MSG}: {rendered:?}"
        );
        assert!(
            rendered.contains(&" 9 - gone".to_owned()),
            "{NUMBERED_MSG}: {rendered:?}"
        );
        assert!(
            rendered.contains(&" 9 + added".to_owned()),
            "{AFTER_GUTTER_MSG}: {rendered:?}"
        );
    }

    /// The `---`/`+++` header names the file twice over, which the heading
    /// already does, so it must not reach the transcript.
    #[test]
    fn a_patch_drops_the_file_header_lines() {
        let rendered = patch_text(&one_file(PATCH)).join("\n");
        assert!(
            !rendered.contains("+++") && !rendered.contains("--- a/"),
            "file headers belong to the wire format: {rendered}"
        );
        assert!(rendered.contains("src/lib.rs +2 -1"), "{HEADING_MSG}");
    }

    #[test]
    fn separate_hunks_are_marked_as_a_jump() {
        let two = "@@ -1,2 +1,2 @@\n first\n+one\n@@ -40,2 +40,2 @@\n second\n+two\n";
        let rendered = patch_text(&one_file(two));
        assert!(
            rendered.iter().any(|l| l.starts_with("...")),
            "{HUNK_GAP_MSG}: {rendered:?}"
        );
    }

    /// A line whose own text starts like a file header arrives after a hunk
    /// header, so it must be kept rather than mistaken for the preamble.
    #[test]
    fn content_that_looks_like_a_file_header_survives() {
        let tricky = "--- a/x\n+++ b/x\n@@ -1,1 +1,2 @@\n keep\n+++ added text\n";
        let rendered = patch_text(&one_file(tricky)).join("\n");
        assert!(
            rendered.contains("++ added text"),
            "content after a hunk header is content: {rendered}"
        );
    }

    const BLOCK_PATCH: &str =
        "@@ -1,4 +1,4 @@\n-alpha\n-beta\n-gamma\n-delta\n+alpha\n+BETA\n+gamma\n+DELTA\n";
    const INTERLEAVED: &[&str] = &[
        "1   alpha",
        "2 - beta",
        "2 + BETA",
        "3   gamma",
        "4 - delta",
        "4 + DELTA",
    ];
    const INTERLEAVED_MSG: &str =
        "a change belongs beside the line it replaces, not after the whole region";
    const NO_NEWLINE_PATCH: &str =
        "@@ -1,2 +1,2 @@\n keep\n-gone\n\\ No newline at end of file\n+added\n";
    const NO_NEWLINE_MSG: &str = "the no-newline marker is not a line of either file";
    const OVERSIZED_MSG: &str = "a hunk too large to diff again is drawn as the wire wrote it";
    const WRAPS: &str = "a changed row must fill the body exactly, or every one of them wraps";
    const NO_TINT: &str = "an unchanged row keeps the card's own background";
    const BOTH_SIDES: &str = "a wide body numbers both sides of a change";
    const ONE_SIDE: &str = "a narrow body spends its columns on the code";
    const MARGIN_ONLY: &str = "a re-indented line is one row marking only its margin";

    /// A changed row is padded to exactly the body width it was given. One
    /// column over and the paragraph wraps it, doubling every line of every
    /// diff; the card's own half of that identity is pinned in `tool_display`.
    #[test_case(76 ; "wide")]
    #[test_case(44 ; "narrow")]
    fn a_changed_row_fills_the_body_without_overflowing_it(body: u16) {
        let lines = render_diff(None, "keep\nold\n", "keep\nnew\n", body);
        let widths: Vec<usize> = lines.iter().map(line_width).collect();

        assert_eq!(
            widths,
            vec![line_width(&lines[0]), body.into(), body.into()],
            "{WRAPS}"
        );
    }

    fn line_width(line: &Line<'static>) -> usize {
        line.spans.iter().map(|s| s.content.width()).sum()
    }

    #[test]
    fn an_unchanged_row_is_not_filled() {
        let lines = render_diff(None, "keep\nold\n", "keep\nnew\n", 80);
        let context = &lines[0];

        assert!(line_width(context) < 80, "{NO_TINT}");
        assert!(
            context.spans.iter().all(|span| span.style.bg.is_none()),
            "{NO_TINT}: {context:?}"
        );
    }

    #[test_case(80, "2 2 ~ " ; "wide body numbers both sides")]
    #[test_case(30, "2 ~ " ; "narrow body numbers one")]
    fn the_gutter_answers_to_the_room_it_has(width: u16, gutter: &str) {
        let lines = render_diff(
            None,
            "fn f() {\nbody\n}\n",
            "fn f() {\n    body\n}\n",
            width,
        );
        let reindented = lines
            .iter()
            .find(|line| line.spans[0].content.contains(MARK_REINDENTED))
            .unwrap_or_else(|| panic!("{MARGIN_ONLY}: {lines:?}"));

        assert_eq!(
            reindented.spans[0].content.as_ref(),
            gutter,
            "{}",
            if width > 60 { BOTH_SIDES } else { ONE_SIDE }
        );
    }

    /// The edit the whole change is shaped around: the body of a block moves
    /// right, and must not come back as a removal and an insertion.
    #[test]
    fn a_wrapped_block_draws_one_row_per_line() {
        let before = "fn f() {\n    work();\n}\n";
        let after = "fn f() {\n    if c {\n        work();\n    }\n}\n";
        let rows: Vec<String> = render_diff(None, before, after, UNCONSTRAINED_WIDTH)
            .iter()
            .map(line_text)
            .collect();

        assert!(
            !rows.iter().any(|row| row.contains(MARK_REMOVED)),
            "{MARGIN_ONLY}: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains(MARK_REINDENTED) && row.ends_with("        work();")),
            "{MARGIN_ONLY}: {rows:?}"
        );
    }

    /// The rows under the file's heading.
    fn patch_rows(patch: &str) -> Vec<String> {
        patch_text(&one_file(patch)).split_off(1)
    }

    /// An applied chunk arrives as the whole region it matched followed by the
    /// whole region it produced, which prints every line they share twice.
    #[test]
    fn a_block_shaped_hunk_is_diffed_again() {
        assert_eq!(patch_rows(BLOCK_PATCH), INTERLEAVED, "{INTERLEAVED_MSG}");
    }

    #[test]
    fn a_no_newline_marker_costs_no_line_number() {
        assert_eq!(
            patch_rows(NO_NEWLINE_PATCH),
            ["1   keep", "2 - gone", "2 + added"],
            "{NO_NEWLINE_MSG}"
        );
    }

    #[test]
    fn an_oversized_hunk_keeps_the_wire_order() {
        let half = MAX_REDIFF_LINES / 2 + 1;
        let removed: String = (0..half).map(|i| format!("-line {i}\n")).collect();
        let added: String = (0..half).map(|i| format!("+line {i}\n")).collect();
        let rendered = patch_rows(&format!("@@ -1,{half} +1,{half} @@\n{removed}{added}"));
        assert_eq!(rendered.len(), half * 2, "{OVERSIZED_MSG}");
        assert!(
            rendered[0].ends_with("- line 0") && rendered[half].ends_with("+ line 0"),
            "{OVERSIZED_MSG}: {:?}",
            &rendered[..1]
        );
    }

    fn diff_fg(lines: &[Line<'static>], substr: &str) -> ratatui::style::Color {
        lines
            .iter()
            .find_map(|l| {
                l.spans
                    .iter()
                    .find(|s| s.content.contains(substr))
                    .and_then(|s| s.style.fg)
            })
            .unwrap_or_else(|| panic!("no fg-styled span containing {substr:?}"))
    }

    /// Walk the file from scratch up to `prefix`, then highlight `text`
    /// and return the fg for `find`. This is our ground truth.
    fn fg_in_context(path: &str, prefix: &str, text: &str, find: &str) -> ratatui::style::Color {
        let mut hl = caudra_highlight::Highlighter::for_path(path);
        for line in prefix.lines() {
            let with_nl = format!("{line}\n");
            let _ = highlight_line(&mut hl, &with_nl);
        }
        let with_nl = format!("{text}\n");
        highlight_line(&mut hl, &with_nl)
            .into_iter()
            .find_map(|span| {
                if span.content.contains(find) {
                    span.style.fg
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("ref fg for {find:?} missing"))
    }

    /// Context lines inside a block comment must carry the full-file parser
    /// state, not a fresh one from the hunk start.
    #[test]
    fn diff_context_line_inside_block_comment_matches_full_file_state() {
        let before = "/*\nalpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\nOLD\ngolf\n*/\n";
        let after = "/*\nalpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\nNEW\ngolf\n*/\n";

        let lines = render_diff(
            Some(caudra_highlight::syntax_for_path("test.rs")),
            before,
            after,
            UNCONSTRAINED_WIDTH,
        );

        let expected = fg_in_context("test.rs", "/*\nalpha\nbravo\ncharlie\n", "delta", "delta");
        assert_eq!(diff_fg(&lines, "delta"), expected);
    }

    /// When an edit removes `*/`, formerly-code lines become comment.
    /// Unchanged context lines must use the AFTER parser state.
    #[test]
    fn diff_unchanged_line_uses_after_state_when_close_tag_removed() {
        let before = "/*\ndoc\n*/\nfn x() {}\n";
        let after = "/*\ndoc\nfn x() {}\n";

        let lines = render_diff(
            Some(caudra_highlight::syntax_for_path("test.rs")),
            before,
            after,
            UNCONSTRAINED_WIDTH,
        );

        let expected = fg_in_context("test.rs", "/*\ndoc\n", "fn x() {}", "fn");
        assert_eq!(diff_fg(&lines, "fn x() {}"), expected);
    }

    #[test]
    fn merge_syntax_with_diff_emphasis_split() {
        let base = Style::new().bg(Color::Red);
        let emph = Style::new().bg(Color::Green);
        let syn = vec![Span::styled("abcde", Style::new().fg(Color::White))];
        let diff = vec![
            plain("abc"),
            DiffSpan {
                text: "de".into(),
                emphasized: true,
            },
        ];
        let result = merge_syntax_with_diff(&syn, &diff, base, emph);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content.as_ref(), "abc");
        assert_eq!(result[0].style.fg, Some(Color::White));
        assert_eq!(result[0].style.bg, Some(Color::Red));
        assert_eq!(result[1].content.as_ref(), "de");
        assert_eq!(result[1].style.bg, Some(Color::Green));
    }

    #[test]
    fn merge_syntax_longer_than_diff_preserves_trailing() {
        let base = Style::new().bg(Color::Red);
        let syn = vec![
            Span::styled("ab", Style::new().fg(Color::Blue)),
            Span::styled("cd", Style::new().fg(Color::Cyan)),
        ];
        let diff = vec![plain("ab")];
        let result = merge_syntax_with_diff(&syn, &diff, base, Style::default());
        assert_eq!(spans_text(&result), "abcd");
    }

    fn grep_entries(files: &[(&str, &[usize])]) -> Vec<GrepFileEntry> {
        files
            .iter()
            .map(|(path, nrs)| GrepFileEntry {
                path: path.to_string(),
                groups: nrs
                    .iter()
                    .map(|&n| GrepMatchGroup::single(n, format!("code at {path}:{n}")))
                    .collect(),
            })
            .collect()
    }

    const GREP_SHAPE: &str = "a grep too big to show has to say how much matched and where";

    #[test_case(&[("a.rs", &[1,2,3,4,5,6,7,8,9,10_usize] as &[usize])], 3, 3 ; "one_file_condenses_to_its_budget")]
    #[test_case(&[("a.rs", &[1_usize,2])],                              5, 2 ; "no_truncation_when_fits")]
    #[test_case(&[("a.rs", &[1_usize,2,3]), ("b.rs", &[10,20])],        4, 3 ; "two_files_name_themselves_then_notice")]
    #[test_case(&[("a.rs", &[1_usize,2])],                              1, 2 ; "the_way_back_outranks_a_single_row")]
    fn render_grep_line_count(files: &[(&str, &[usize])], max: usize, expected: usize) {
        let entries = grep_entries(files);
        assert_eq!(
            render_grep_results(&entries, None, max, true).0.len(),
            expected
        );
    }

    fn grep_text(files: &[(&str, &[usize])], max: usize) -> Vec<String> {
        render_grep_results(&grep_entries(files), None, max, false)
            .0
            .iter()
            .map(line_text)
            .collect()
    }

    /// A search that stopped at a bound read part of the tree, so an absent
    /// match is not evidence that there is none.
    #[test]
    fn a_capped_grep_says_how_far_it_searched() {
        let entries = grep_entries(&[("a.rs", &[1_usize])]);
        let cap = SearchCap {
            files_scanned: 40,
            files_listed: 900,
        };
        let capped: Vec<String> = render_grep_results(&entries, Some(&cap), 10, false)
            .0
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            capped.last().map(String::as_str),
            Some("searched 40 of 900 files; more matches may exist")
        );
        assert_eq!(
            capped.len(),
            grep_text(&[("a.rs", &[1_usize])], 10).len() + 1
        );
    }

    /// The question a grep answers is how much matched and where, which the
    /// first few matches in document order cannot say. The totals belong to
    /// the card header, which already annotates itself with them, so spending
    /// a row repeating them buys nothing.
    #[test]
    fn a_condensed_grep_leads_with_its_shape() {
        let rendered = grep_text(
            &[
                ("a.rs", &[1, 2, 3, 4_usize] as &[usize]),
                ("b.rs", &[10_usize]),
                ("c.rs", &[20_usize, 30]),
            ],
            4,
        );
        assert_eq!(rendered[0], "  a.rs \u{b7} 4", "{GREP_SHAPE}");
        assert_eq!(rendered[1], "  b.rs \u{b7} 1", "{GREP_SHAPE}");
        assert_eq!(rendered[2], "  c.rs \u{b7} 2", "{GREP_SHAPE}");
        assert!(
            rendered.last().unwrap().contains(EXPAND_AFFORDANCE),
            "the detail stays one click away"
        );
    }

    /// The count used to be of matches and the word was always "lines".
    #[test_case(4,  "3 files"   ; "names_the_files_it_left_out")]
    #[test_case(10, "10 matches" ; "names_the_matches_when_every_file_fits")]
    fn a_condensed_grep_counts_what_expanding_would_add(max: usize, gained: &str) {
        let rendered = grep_text(
            &[
                ("a.rs", &[1, 2, 3, 4_usize] as &[usize]),
                ("b.rs", &[10_usize]),
                ("c.rs", &[20_usize, 30]),
                ("d.rs", &[40_usize]),
                ("e.rs", &[50_usize]),
                ("f.rs", &[60_usize]),
            ],
            max,
        );
        assert_eq!(rendered.last().unwrap(), &expand_notice(gained));
    }

    /// One file has no distribution to summarise, so it names itself and the
    /// matches keep the room.
    #[test]
    fn a_condensed_single_file_grep_still_shows_matches() {
        let rendered = grep_text(&[("a.rs", &[1, 2, 3, 4, 5_usize] as &[usize])], 4);
        assert_eq!(rendered[0], "a.rs");
        assert!(rendered[1].contains("code at a.rs:1"));
        assert_eq!(rendered.last().unwrap(), &expand_notice("3 lines"));
    }

    /// Expanding is what the affordance promised, so it has to give the match
    /// text back rather than a roomier summary.
    #[test]
    fn expanding_a_grep_returns_the_matches_themselves() {
        let files: &[(&str, &[usize])] = &[("a.rs", &[1, 2, 3_usize]), ("b.rs", &[10_usize, 20])];
        let rendered = grep_text(files, usize::MAX);
        assert!(rendered.iter().any(|l| l.contains("code at a.rs:3")));
        assert!(rendered.iter().any(|l| l.contains("code at b.rs:20")));
        assert!(!rendered.iter().any(|l| l.contains(EXPAND_AFFORDANCE)));
    }

    fn spans_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_text(line: &Line) -> String {
        spans_text(&line.spans)
    }

    #[test]
    fn multi_file_grep_headers_and_alignment() {
        let entries = grep_entries(&[("a.rs", &[1]), ("b.rs", &[100])]);
        let (lines, _) = render_grep_results(&entries, None, 10, false);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("a.rs")));
        assert!(texts.iter().any(|t| t.contains("b.rs")));

        let gutter_width =
            |line: &str| line.find(|c: char| c.is_alphabetic()).unwrap_or(usize::MAX);
        let content_gutters: Vec<usize> = texts
            .iter()
            .filter(|t| !t.contains(".rs"))
            .map(|t| gutter_width(t))
            .collect();
        assert!(
            content_gutters.windows(2).all(|w| w[0] == w[1]),
            "gutter widths should be uniform across files: {content_gutters:?}"
        );
    }

    #[test_case(MAX_INSTRUCTION_LINES, true,  true  ; "collapsed_truncates")]
    #[test_case(usize::MAX,             false, false ; "expanded_shows_all")]
    fn render_instructions_truncation(
        max_lines: usize,
        expect_truncated: bool,
        expect_notice: bool,
    ) {
        let long_content: String = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: long_content,
        }];
        let mut lines = Vec::new();
        let truncated = render_instructions(&blocks, &mut lines, max_lines, false);
        assert_eq!(truncated, expect_truncated);
        let has_notice = lines
            .iter()
            .any(|l| line_text(l).contains(TRUNCATION_PREFIX));
        assert_eq!(has_notice, expect_notice);
    }

    #[test]
    fn render_instructions_empty_content() {
        let blocks = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: String::new(),
        }];
        let mut lines = Vec::new();
        let truncated = render_instructions(&blocks, &mut lines, MAX_INSTRUCTION_LINES, false);
        assert!(!truncated);
        assert_eq!(lines.len(), 0);
    }

    fn index_line(
        output_line: usize,
        text: &str,
        semantic: IndexLineSemantic,
        body: Option<&str>,
        source_range: Option<(usize, usize)>,
    ) -> IndexLine {
        IndexLine {
            output_line,
            text: text.into(),
            semantic,
            body: body.map(str::to_owned),
            source_range: source_range.map(|(start_line, end_line)| IndexSourceRange {
                start_line,
                end_line,
            }),
        }
    }

    #[test]
    fn index_file_renders_semantics_ranges_and_declaration_highlights() {
        let source = vec![
            index_line(1, "fns:", IndexLineSemantic::Section, None, None),
            index_line(
                2,
                "  pub run() [10-12]",
                IndexLineSemantic::Item,
                Some("  pub run()"),
                Some((10, 12)),
            ),
            index_line(
                3,
                "  [2 more truncated]",
                IndexLineSemantic::Dimmed,
                None,
                None,
            ),
        ];

        let (lines, truncated) = render_index_file("rust", &source, usize::MAX, true);

        assert!(!truncated);
        assert_eq!(lines[0].spans[0].style, theme::current().index_section);
        assert_eq!(
            lines[1].spans.last().unwrap().style,
            theme::current().index_line_nr
        );
        assert_eq!(line_text(&lines[1]), "  pub run() [10-12]");
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.content.contains("pub") && span.style != theme::current().tool)
        );
        assert_eq!(lines[2].spans[0].style, theme::current().tool_dim);
    }

    #[test]
    fn index_file_and_directory_use_head_caps() {
        let source = (1..=4)
            .map(|line| {
                index_line(
                    line,
                    &format!("fn item_{line}() [{line}]"),
                    IndexLineSemantic::Item,
                    Some(&format!("fn item_{line}()")),
                    Some((line, line)),
                )
            })
            .collect::<Vec<_>>();
        let (file, file_truncated) = render_index_file("rust", &source, 1, false);
        assert!(file_truncated);
        assert!(line_text(&file[0]).contains("item_1"));
        assert!(line_text(file.last().unwrap()).contains(TRUNCATION_PREFIX));

        let directory = IndexOutput::Directory {
            path: "/tmp".into(),
            relative_path: ".".into(),
            entries: vec![
                caudra_agent::IndexDirectoryEntry {
                    name: "src".into(),
                    kind: IndexDirectoryEntryKind::Directory,
                },
                caudra_agent::IndexDirectoryEntry {
                    name: "a.rs".into(),
                    kind: IndexDirectoryEntryKind::File,
                },
                caudra_agent::IndexDirectoryEntry {
                    name: "b.rs".into(),
                    kind: IndexDirectoryEntryKind::File,
                },
            ],
            total_count: 3,
            truncated: false,
            listing: "src/\na.rs\nb.rs".into(),
            instructions: None,
            state: None,
        };
        let (directory, directory_truncated) = render_index_directory(&directory, 1);
        assert!(directory_truncated);
        assert_eq!(line_text(&directory[0]), "src/");
        assert_eq!(directory[0].spans[0].style, theme::current().tool_path);
    }

    #[test]
    fn truncated_directory_marker_is_visible_when_collapsed_and_expanded() {
        let directory = IndexOutput::Directory {
            path: "/tmp".into(),
            relative_path: ".".into(),
            entries: vec![caudra_agent::IndexDirectoryEntry {
                name: "src".into(),
                kind: IndexDirectoryEntryKind::Directory,
            }],
            total_count: 2,
            truncated: true,
            listing: "src/".into(),
            instructions: None,
            state: Some(serde_json::json!({"truncated": true})),
        };

        for max_lines in [1, usize::MAX] {
            let (lines, _) = render_index_directory(&directory, max_lines);
            assert_eq!(line_text(lines.last().unwrap()), INDEX_TRUNCATED);
            assert_eq!(
                lines.last().unwrap().spans[0].style,
                theme::current().tool_dim
            );
        }
    }

    #[test_case("héllo",           &["hé", "llo"]          ; "accented")]
    #[test_case("🦀x",              &["🦀", "x"]            ; "emoji")]
    #[test_case("sep := \"│\"",     &["sep := \"", "│\""]   ; "box_drawing")]
    #[test_case("日本語",           &["日本", "語"]         ; "cjk")]
    fn merge_syntax_with_diff_multibyte(input: &str, parts: &[&str]) {
        let base = Style::new().bg(Color::Red);
        let emph = Style::new().bg(Color::Green);
        let syn = vec![Span::styled(
            input.to_owned(),
            Style::new().fg(Color::White),
        )];
        let diff: Vec<DiffSpan> = parts
            .iter()
            .enumerate()
            .map(|(i, &t)| DiffSpan {
                text: t.into(),
                emphasized: i == 0,
            })
            .collect();
        let result = merge_syntax_with_diff(&syn, &diff, base, emph);
        assert_eq!(spans_text(&result), input);
    }
    const CHILD_BODY: &str = "child body line";
    const EXPECT_ROW: &str = "the summary row has to name its child";
    /// What `batch` itself resolves to. A child never reads it: it rests at
    /// its own tool's budget, or draws whole once asked for.
    const PARENT_BUDGET: usize = 3;
    /// The budgets the children rest at, which are the ones the reader's own
    /// card would use.
    const TOOL_LINES: ToolOutputLines = ToolOutputLines::DEFAULT;

    /// Read-only unless a case says otherwise: a call that changed nothing is
    /// the one the card folds, which is what most of these are about.
    fn batch_entry(tool: &str, body_lines: usize) -> BatchToolEntry {
        let text = (0..body_lines)
            .map(|i| format!("{CHILD_BODY} {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        BatchToolEntry {
            tool: tool.into(),
            effect: ToolEffect::ReadOnly,
            summary: format!("{tool} summary"),
            status: BatchToolStatus::Success,
            input: None,
            raw_input: None,
            output: Some(ToolOutput::Plain(caudra_agent::TextOutput {
                text,
                instructions: None,
                state: None,
                lua_provenance: None,
            })),
            annotation: None,
        }
    }

    fn limits(views: BatchViews) -> RenderLimits {
        RenderLimits::new(false, PARENT_BUDGET, views, TOOL_LINES)
    }

    const MARKDOWN_CHILD_TOOL: &str = "task";
    const FENCE_MARK: &str = "```";
    const FENCE_KEY: &str = "answer";
    /// One value long enough that a narrow card has to break the line, which
    /// is what a subagent's structured report looks like.
    const FENCE_PAYLOAD: &str =
        "found the middleware, the router, and the two call sites that bypass both";
    const CODE_GUTTER: &str = caudra_markdown::render::CODE_BAR_WRAP;
    const NARROW_BODY_WIDTH: u16 = 24;

    /// A fenced block the line budget cut before its closing fence, which is
    /// what `truncate_output` hands the card for any structured task result.
    fn unclosed_fence() -> String {
        format!("{FENCE_MARK}json\n{{\n  \"{FENCE_KEY}\": \"{FENCE_PAYLOAD}\"")
    }

    /// A block that fits occupies the blank row the renderer opens with, then
    /// one row per line of code; the fence itself draws none.
    fn unbroken_rows(fence: &str) -> usize {
        fence.lines().count()
    }

    fn markdown_entry(text: &str) -> BatchToolEntry {
        BatchToolEntry {
            tool: MARKDOWN_CHILD_TOOL.into(),
            effect: ToolEffect::ReadOnly,
            summary: format!("{MARKDOWN_CHILD_TOOL} summary"),
            status: BatchToolStatus::Success,
            input: None,
            raw_input: None,
            output: Some(ToolOutput::Markdown(caudra_agent::TextOutput {
                text: text.to_owned(),
                instructions: None,
                state: None,
                lua_provenance: None,
            })),
            annotation: None,
        }
    }

    /// The child's body rows, which follow its one summary row. The card is
    /// given the child's width plus the indent it prefixes, so `width` is what
    /// the body itself ends up with.
    fn markdown_child_body(text: &str, width: u16) -> Vec<String> {
        let limits = limits(BatchViews::new([0])).with_width(width + BATCH_CHILD_INDENT_WIDTH);
        let (lines, ..) = render_batch(&[markdown_entry(text)], false, &limits);
        lines
            .iter()
            .skip(1)
            .map(|line| spans_text(&line.spans))
            .collect()
    }

    /// A line the card does not break is one the terminal breaks, and `Wrap`
    /// restarts a continuation at column zero, so the row stops saying whose
    /// body it is.
    #[test]
    fn a_wrapped_markdown_child_indents_every_line() {
        let fence = unclosed_fence();
        let body = markdown_child_body(&fence, NARROW_BODY_WIDTH);
        assert!(
            body.len() > unbroken_rows(&fence),
            "the long value has to have been broken: {body:?}"
        );
        let limit = usize::from(NARROW_BODY_WIDTH) + BATCH_CHILD_INDENT.len();
        for line in &body {
            assert!(
                line.starts_with(BATCH_CHILD_INDENT),
                "an unindented row reads as belonging to the batch, not the child: {line:?}"
            );
            assert!(
                line.chars().count() <= limit,
                "a row past the width is one the terminal wraps for us: {line:?}"
            );
        }
    }

    /// The escape hatch every caller with no width to give relies on.
    #[test]
    fn a_markdown_child_with_no_width_is_left_unbroken() {
        let fence = unclosed_fence();
        let body = markdown_child_body(&fence, UNCONSTRAINED_WIDTH);
        assert_eq!(body.len(), unbroken_rows(&fence), "{body:?}");
    }

    /// The budget cuts the text before the renderer sees it, so the closing
    /// fence is routinely missing. It still has to read as a block rather than
    /// put its own syntax on screen.
    #[test]
    fn a_fence_the_budget_cut_short_does_not_leak_its_syntax() {
        let body = markdown_child_body(&unclosed_fence(), NARROW_BODY_WIDTH);
        assert!(
            body.iter().any(|line| line.contains(FENCE_KEY)),
            "the payload has to survive: {body:?}"
        );
        assert!(
            !body.iter().any(|line| line.contains(FENCE_MARK)),
            "the fence is presentation, not content: {body:?}"
        );
        assert!(
            body.iter().any(|line| line.contains(CODE_GUTTER)),
            "an unterminated fence still has to read as a block: {body:?}"
        );
    }

    fn batch_of(
        sizes: [usize; 2],
        views: BatchViews,
    ) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>) {
        let entries = [batch_entry("read", sizes[0]), batch_entry("grep", sizes[1])];
        let (lines, rows, _) = render_batch(&entries, false, &limits(views));
        (lines, rows)
    }

    fn batch(views: BatchViews) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>) {
        batch_of([2, 3], views)
    }

    fn body_count(lines: &[Line<'static>]) -> usize {
        lines
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains(CHILD_BODY))
            })
            .count()
    }

    fn targets(rows: &[Option<RowTarget>]) -> Vec<RowTarget> {
        rows.iter().flatten().copied().collect()
    }

    fn unique_targets(rows: &[Option<RowTarget>]) -> Vec<RowTarget> {
        let mut seen: Vec<RowTarget> = Vec::new();
        for target in rows.iter().flatten() {
            if seen.last() != Some(target) {
                seen.push(*target);
            }
        }
        seen
    }

    /// The point of the card: a batch nobody has clicked is the list of what
    /// it ran, not every result at once.
    #[test]
    fn an_untouched_batch_draws_no_child_bodies() {
        let (lines, rows) = batch(BatchViews::default());
        assert_eq!(body_count(&lines), 0);
        assert_eq!(
            lines.len(),
            rows.len(),
            "the rows are parallel to the lines"
        );
        assert_eq!(
            targets(&rows),
            vec![RowTarget(0), RowTarget(1)],
            "{EXPECT_ROW}"
        );
    }

    #[test]
    fn opening_a_child_shows_only_its_body() {
        let (lines, rows) = batch(BatchViews::new([0]));
        assert_eq!(body_count(&lines), 2, "the other child stays folded");
        assert_eq!(lines.len(), rows.len());
        assert_eq!(
            targets(&rows),
            vec![RowTarget(0), RowTarget(0), RowTarget(0), RowTarget(1)],
            "every row of an open child answers for it, folded rows for theirs"
        );
    }

    /// A row with nothing under it must not read the same as one whose body
    /// was put away.
    #[test]
    fn a_folded_child_says_so() {
        let marked = |lines: &[Line<'static>]| {
            lines
                .iter()
                .filter(|line| {
                    line.spans
                        .iter()
                        .any(|span| span.content.contains(BATCH_FOLDED_MARK))
                })
                .count()
        };
        assert_eq!(marked(&batch(BatchViews::default()).0), 2);
        let (one_open, _) = batch(BatchViews::new([0]));
        assert_eq!(marked(&one_open), 1);
    }

    /// The target names the child, so a click after a fold above it still
    /// reaches the one the reader aimed at.
    #[test]
    fn a_summary_row_names_its_own_child() {
        let (_, rows) = batch(BatchViews::new([1]));
        assert_eq!(unique_targets(&rows), vec![RowTarget(0), RowTarget(1)]);
    }

    /// A child is folded or whole, so a click anywhere in an open one puts it
    /// back rather than asking a question the body has already answered.
    #[test]
    fn every_row_of_an_open_child_takes_a_click() {
        let (lines, rows) = batch_of([1, 6], BatchViews::new([1]));
        assert!(
            !lines
                .iter()
                .any(|line| line_text(line).contains(TRUNCATION_PREFIX)),
            "an opened child draws whole, so it has nothing to announce"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| **row == Some(RowTarget(1)))
                .count(),
            1 + 6,
            "the summary row and every body row name the same child"
        );
    }

    /// Opening names one child, and says nothing about the others.
    #[test]
    fn opening_a_child_leaves_its_siblings_folded() {
        let whole = 6;
        let (lines, rows) = batch_of([5, whole], BatchViews::new([1]));
        assert_eq!(
            body_count(&lines),
            whole,
            "the opened child is whole and the other is still put away"
        );
        assert!(
            unique_targets(&rows).contains(&RowTarget(0)),
            "a folded sibling stays clickable"
        );
    }

    #[test_case(&[], 1, &[1] ; "a_folded_child_opens")]
    #[test_case(&[1], 1, &[] ; "an_open_child_folds_again")]
    #[test_case(&[0], 1, &[0, 1] ; "leaves_the_others")]
    fn toggled_views(start: &[usize], index: usize, expected: &[usize]) {
        let views = BatchViews::new(start.iter().copied()).toggled(index);
        assert_eq!(views.0.as_ref(), expected);
    }

    /// A nested batch has its own roster, so the parent's views must not
    /// reach it.
    #[test]
    fn views_do_not_reach_a_nested_batch() {
        let child = limits(BatchViews::new([0]))
            .child(0, &batch_entry("read", 1))
            .expect("an asked-for child draws");
        assert_eq!(child.views, BatchViews::default());
    }

    /// Asking for a child is asking for all of it. A budget is where a child
    /// rests, never where a click lands, so opening one answers the question
    /// the click asked rather than half of it.
    #[test]
    fn an_opened_child_draws_whole() {
        let whole = 6;
        let (lines, _) = batch_of([whole, whole], BatchViews::new([0, 1]));
        assert_eq!(
            body_count(&lines),
            whole * 2,
            "an opened child spends no budget at all"
        );
    }

    const GREP_CHILD: &str = "grep";
    const GIVEN_ANNOTATION: &str = "cached";
    const CHILD_COUNT_MSG: &str =
        "a child row reports what its result holds, the way a standalone row does";

    fn child_row(entry: BatchToolEntry) -> String {
        line_text(&render_batch(&[entry], false, &limits(BatchViews::default())).0[0])
    }

    fn child_sigil(entry: BatchToolEntry) -> Span<'static> {
        render_batch(&[entry], false, &limits(BatchViews::default())).0[0].spans[0].clone()
    }

    /// The reported bug: a finished grep named no matches. `batch` copies only
    /// the dispatch's annotation, which grep never sets, so the row that
    /// answers how much matched was blank on the one tool whose entire result
    /// is a count.
    #[test_case(BatchToolStatus::Success, None, Some("(3 matches in 2 files)") ; "the result speaks for itself")]
    #[test_case(BatchToolStatus::Success, Some(GIVEN_ANNOTATION), Some("(cached)") ; "a given annotation still wins")]
    #[test_case(BatchToolStatus::Error, None, None ; "a failure is named by its own body")]
    fn a_child_row_annotates_what_its_result_holds(
        status: BatchToolStatus,
        given: Option<&str>,
        expected: Option<&str>,
    ) {
        let row = child_row(BatchToolEntry {
            status,
            annotation: given.map(str::to_owned),
            output: Some(ToolOutput::GrepResult {
                entries: grep_entries(&[("a.rs", &[1, 2_usize]), ("b.rs", &[3_usize])]),
                capped: None,
            }),
            ..batch_entry(GREP_CHILD, 0)
        });

        match expected {
            Some(annotation) => assert!(row.contains(annotation), "{CHILD_COUNT_MSG}: {row:?}"),
            None => assert!(!row.contains('('), "{CHILD_COUNT_MSG}: {row:?}"),
        }
    }

    const SHELL_CHILD: &str = "shell";
    const SHELL_WIRE_CHILD: &str = "mcp_Shell";
    const SHELL_CHILD_ROW: &str = "$ Ran";
    const UNTABLED_CHILD: &str = "srv.custom";
    const UNTABLED_CHILD_ROW: &str = "⚙ srv.custom";
    const CHILD_LABEL_MSG: &str = "a child names its tool the way a standalone compact row does";

    /// A child row printed the string the model called, so a batch run under
    /// Anthropic OAuth read `mcp_Shell>` where the same call on its own read
    /// `$ Shell`. The roster carries the resolved name now, and the table
    /// answers for a qualified one either way.
    #[test_case(SHELL_CHILD, SHELL_CHILD_ROW ; "a tabled tool answers with its row")]
    #[test_case(SHELL_WIRE_CHILD, SHELL_CHILD_ROW ; "a wire name reaches the same row")]
    #[test_case(UNTABLED_CHILD, UNTABLED_CHILD_ROW ; "an untabled tool answers with itself")]
    fn a_child_row_names_its_tool_like_a_compact_row(tool: &str, expected: &str) {
        let row = child_row(batch_entry(tool, 0));
        assert!(
            row.starts_with(&format!("{expected} ")),
            "{CHILD_LABEL_MSG}: {row:?}"
        );
    }

    const SIGIL_MSG: &str = "a child opens on its sigil, which is what carries the outcome";

    /// The row used to lead with a status dot and then repeat the outcome in
    /// the sigil that followed it, which a standalone compact row has never
    /// done. Pending and running are still told apart from a finished call
    /// without it, by the tense of the label beside the sigil.
    #[test_case(BatchToolStatus::Pending, "$ Run"     ; "pending")]
    #[test_case(BatchToolStatus::Running, "$ Running" ; "running")]
    #[test_case(BatchToolStatus::Success, "$ Ran"     ; "success")]
    #[test_case(BatchToolStatus::Error,   "$ Run"     ; "error")]
    fn a_child_row_leads_with_its_sigil(status: BatchToolStatus, expected: &str) {
        let row = child_row(BatchToolEntry {
            status,
            ..batch_entry(SHELL_CHILD, 0)
        });
        assert!(row.starts_with(expected), "{SIGIL_MSG}: {row:?}");
    }

    fn shell_child(status: BatchToolStatus) -> BatchToolEntry {
        BatchToolEntry {
            status,
            ..batch_entry(SHELL_CHILD, 0)
        }
    }

    /// Colour is the only thing left saying how a child went, so every state
    /// has to reach the row with one of its own.
    #[test]
    fn every_child_state_paints_its_sigil_apart() {
        let styles = [
            BatchToolStatus::Pending,
            BatchToolStatus::Running,
            BatchToolStatus::Success,
            BatchToolStatus::Error,
        ]
        .map(|status| child_sigil(shell_child(status)).style);

        for (i, style) in styles.iter().enumerate() {
            for other in &styles[i + 1..] {
                assert_ne!(style, other, "{SIGIL_MSG}");
            }
        }
    }

    /// The same rule a standalone row runs, reached through the child's own
    /// structured output rather than through its status, which says only that
    /// the search completed.
    #[test]
    fn a_child_that_found_nothing_is_not_painted_as_a_hit() {
        let found_nothing = child_sigil(BatchToolEntry {
            status: BatchToolStatus::Success,
            output: Some(ToolOutput::GrepResult {
                entries: Vec::new(),
                capped: None,
            }),
            ..batch_entry(GREP_CHILD, 0)
        });
        let found_something = child_sigil(BatchToolEntry {
            status: BatchToolStatus::Success,
            output: Some(ToolOutput::GrepResult {
                entries: grep_entries(&[("a.rs", &[1_usize])]),
                capped: None,
            }),
            ..batch_entry(GREP_CHILD, 0)
        });

        assert_ne!(found_nothing.style, found_something.style, "{SIGIL_MSG}");
    }

    const CHILD_TIMEOUT: u32 = 120_000;
    const CHILD_TIMEOUT_SHOWN: &str = "timeout=2m";
    const COMMAND_KEY: &str = "command=";
    const CHILD_ARGS_MSG: &str = "a child's brackets carry what its own header does not show";

    /// The same filter a standalone row uses, reached through the same table,
    /// so a child cannot print the command it has already drawn. The timeout
    /// is there to prove the brackets are still drawn at all, and that a child
    /// reaches the same table a standalone row does to read its unit.
    #[test]
    fn a_child_row_never_repeats_its_header_in_brackets() {
        let mut entry = batch_entry(SHELL_WIRE_CHILD, 0);
        let summary = entry.summary.clone();
        entry.raw_input = Some(serde_json::json!({
            "command": summary,
            "timeout": CHILD_TIMEOUT,
        }));

        let row = child_row(entry);
        assert!(!row.contains(COMMAND_KEY), "{CHILD_ARGS_MSG}: {row:?}");
        assert!(
            row.contains(CHILD_TIMEOUT_SHOWN),
            "{CHILD_ARGS_MSG}: {row:?}"
        );
    }

    const BATCH_TIGHT_MSG: &str = "a roster of folded calls must stack like the list it is";
    const BATCH_BODY_AIR_MSG: &str = "a child carrying a body must be set off from its neighbours";

    fn blank_rows(lines: &[Line<'static>]) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line_text(line).is_empty())
            .map(|(row, _)| row)
            .collect()
    }

    /// Every child was separated by a blank row whether or not it had anything
    /// under it, so a roster of folded calls was drawn at twice its height and
    /// read as unrelated cards rather than one list. Outside a batch the same
    /// rows stack flush.
    #[test]
    fn folded_children_stack_the_way_the_transcript_stacks_them() {
        let (lines, rows) = batch(BatchViews::default());

        assert!(blank_rows(&lines).is_empty(), "{BATCH_TIGHT_MSG}");
        assert_eq!(lines.len(), rows.len(), "the rows stay parallel");
    }

    /// The other half of the transcript's rule: a row with a body is no longer
    /// a list entry, and running it flush into its neighbours hides where the
    /// body starts and ends.
    #[test]
    fn a_child_with_a_body_keeps_the_air_around_it() {
        let entries = [
            batch_entry("read", 1),
            batch_entry("read", 2),
            batch_entry("read", 1),
        ];
        let (lines, rows, _) = render_batch(&entries, false, &limits(BatchViews::new([1])));

        assert_eq!(blank_rows(&lines), vec![1, 5], "{BATCH_BODY_AIR_MSG}");
        assert_eq!(lines.len(), rows.len(), "the rows stay parallel");
    }

    /// The card's own body is the list of what it ran, so opening it says
    /// nothing about the children. Otherwise one click on a card of ten greps
    /// would write ten whole greps into the transcript.
    #[test]
    fn an_opened_card_leaves_its_children_folded() {
        let whole = 6;
        let opened = RenderLimits::new(true, PARENT_BUDGET, BatchViews::default(), TOOL_LINES);
        let entries = [batch_entry("read", whole), batch_entry("grep", whole)];
        let (lines, ..) = render_batch(&entries, false, &opened);
        assert_eq!(body_count(&lines), 0);
    }

    const WRITE_CHILD: &str = "file_write";
    /// What a write rests at on a card of its own, which is what a child of a
    /// batch has to rest at too.
    const WRITE_BUDGET: usize = ToolOutputLines::DEFAULT.write;
    const CHANGED_MSG: &str =
        "a child whose body is the only record of what it did draws with the card";
    const CHILD_BUDGET_MSG: &str = "an unasked child rests where its own card would rest";

    fn write_entry(body_lines: usize) -> BatchToolEntry {
        BatchToolEntry {
            effect: ToolEffect::Mutating,
            ..batch_entry(WRITE_CHILD, body_lines)
        }
    }

    fn write_batch(
        body_lines: usize,
        views: BatchViews,
    ) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>) {
        let (lines, rows, _) = render_batch(&[write_entry(body_lines)], false, &limits(views));
        (lines, rows)
    }

    /// The reported bug: a batch of edits drew a roster of headers and no
    /// diffs, while every one of those edits outside a batch keeps its body in
    /// all three view modes. Folding them away loses the change.
    #[test]
    fn a_child_that_changed_something_draws_without_a_click() {
        let (lines, _) = write_batch(2, BatchViews::default());

        assert_eq!(body_count(&lines), 2, "{CHANGED_MSG}");
        assert!(
            !lines
                .iter()
                .any(|line| line_text(line).contains(BATCH_FOLDED_MARK)),
            "nothing was put away, so nothing may claim it was"
        );
    }

    /// `shell` changes things too, and its header names the whole command down
    /// to the exit status, so it folds where a write does not.
    #[test]
    fn a_shell_child_still_folds() {
        let entry = BatchToolEntry {
            effect: ToolEffect::Mutating,
            ..batch_entry(SHELL_CHILD, 2)
        };

        let (lines, ..) = render_batch(&[entry], false, &limits(BatchViews::default()));

        assert_eq!(body_count(&lines), 0, "{CHANGED_MSG}");
    }

    /// Otherwise a batch of five writes is five whole files, which is the
    /// flood the card exists to prevent.
    #[test]
    fn an_unasked_child_rests_at_its_own_tools_budget() {
        let (lines, rows) = write_batch(WRITE_BUDGET * 2, BatchViews::default());

        assert_eq!(body_count(&lines), WRITE_BUDGET, "{CHILD_BUDGET_MSG}");
        assert!(
            lines
                .iter()
                .any(|line| line_text(line).contains(TRUNCATION_PREFIX)),
            "a child holding something back has to say so: {CHILD_BUDGET_MSG}"
        );
        assert!(
            unique_targets(&rows).contains(&RowTarget(0)),
            "and the row it says it on has to take the click"
        );
    }

    #[test]
    fn asking_for_a_resting_child_draws_it_whole() {
        let (lines, rows) = write_batch(WRITE_BUDGET * 2, BatchViews::new([0]));

        assert_eq!(body_count(&lines), WRITE_BUDGET * 2, "{CHANGED_MSG}");
        assert!(
            unique_targets(&rows).contains(&RowTarget(0)),
            "an opened child stays a control, or it could not be put back"
        );
    }

    /// A dispatched child is traced back to its subagent through the rows it
    /// owns, so every row of one keeps naming it however it was drawn.
    #[test]
    fn every_row_of_a_resting_child_still_names_it() {
        let (lines, rows) = write_batch(2, BatchViews::default());

        assert_eq!(lines.len(), rows.len(), "the rows stay parallel");
        assert_eq!(unique_targets(&rows), vec![RowTarget(0)]);
    }

    fn ranked_row(name: &str, inbound: usize) -> CodeGraphRow {
        CodeGraphRow {
            name: name.to_owned(),
            kind: "function".to_owned(),
            path: "src/lib.rs".to_owned(),
            line_start: 10,
            line_end: 20,
            inbound: Some(inbound),
            outbound: Some(1),
            hops: None,
            test_scope: false,
        }
    }

    #[test]
    fn a_code_graph_card_keeps_its_headline_and_footer() {
        const HEADLINE: &str = "ranked symbols in .";
        const FOOTER: &str = "[3 files, 9 symbols, 4 edges]";

        let rows = vec![ranked_row("alpha", 3), ranked_row("beta", 1)];
        let (lines, truncated) = render_code_graph(HEADLINE, &rows, None, FOOTER, 20, false);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(!truncated);
        assert_eq!(texts.first().map(String::as_str), Some(HEADLINE));
        assert_eq!(texts.last().map(String::as_str), Some(FOOTER));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("alpha") && t.contains("in   3"))
        );
    }

    /// The two lines that make the rows interpretable are the two a shortened
    /// card must still show.
    #[test]
    fn a_shortened_card_keeps_the_lines_that_explain_the_rest() {
        const HEADLINE: &str = "ranked symbols in .";
        const FOOTER: &str = "[100 files]";

        let rows: Vec<CodeGraphRow> = (0..40).map(|i| ranked_row(&format!("sym{i}"), i)).collect();
        let (lines, truncated) = render_code_graph(HEADLINE, &rows, None, FOOTER, 8, false);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(truncated);
        assert_eq!(texts.first().map(String::as_str), Some(HEADLINE));
        assert_eq!(texts.last().map(String::as_str), Some(FOOTER));
    }

    /// A rank caught mid-descent orders the same way a converged one does. The
    /// footer is the only place that difference is visible, so it has to look
    /// different too.
    #[test]
    fn a_footer_naming_a_caveat_is_not_dimmed_like_an_ordinary_one() {
        let rows = vec![ranked_row("alpha", 1)];
        let plain = render_code_graph("h", &rows, None, "[3 files]", 20, false).0;
        let caveated = render_code_graph(
            "h",
            &rows,
            None,
            "[3 files; rank NOT converged after 50 iterations]",
            20,
            false,
        )
        .0;

        assert_ne!(
            plain.last().expect("footer").spans[0].style,
            caveated.last().expect("footer").spans[0].style
        );
    }

    #[test]
    fn a_reach_row_leads_with_its_hop_distance() {
        let mut near = ranked_row("near", 0);
        near.inbound = None;
        near.outbound = None;
        near.hops = Some(1);
        let mut far = near.clone();
        far.name = "far".to_owned();
        far.hops = Some(12);

        let (lines, _) = render_code_graph("h", &[near, far], None, "[f]", 20, false);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        let hop_column = |needle: &str| -> String {
            let row = texts.iter().find(|t| t.contains(needle)).expect("row");
            row.chars().take_while(|c| !c.is_alphabetic()).collect()
        };
        assert_eq!(
            hop_column("near").len(),
            hop_column("far").len(),
            "a two-digit hop must not shift the name column of a one-digit row"
        );
        assert!(hop_column("far").contains("12"));
    }

    #[test]
    fn code_expand_renders_the_body_under_its_path() {
        let source = CodeGraphSource {
            path: "src/lib.rs".to_owned(),
            kind: "function".to_owned(),
            line_start: 10,
            lines: vec!["fn alpha() {".to_owned(), "}".to_owned()],
            whole_file_reason: Some("the bundle would cost more than the file".to_owned()),
        };
        let (lines, _) = render_code_graph("h", &[], Some(&source), "[f]", 20, false);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("bundle would cost more")),
            "a whole-file substitution has to say why"
        );
        assert!(texts.iter().any(|t| t.contains("fn alpha")));
    }
}
