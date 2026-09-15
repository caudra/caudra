use std::collections::HashMap;
use std::iter;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::highlight::{fallback_span, highlight_line};
use crate::markdown::{expand_notice, should_truncate, text_to_wrapped, truncation_notice};
use crate::provenance::LineProvenance;
use crate::selection::wrap_breaks;
use crate::theme;

use super::tool_display::{
    ScrollTail, TREE_BRANCH, TREE_GAP, TREE_LAST, TREE_TRUNK, activity_child_spans,
    activity_detail, activity_label, activity_sigil, batch_sigil_style, compact_args_for,
    compact_sigil_label, header_spans, inflected_header, names_tool, scroll_footer_text,
};
use super::{ToolProgress, environment_card, is_collapsible, workflow_card};
use caudra_agent::tools::{PYTHON_EXECUTION_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME};
use caudra_agent::types::Answer;
use caudra_agent::types::{TodoItem, TodoStatus};
use caudra_agent::{
    BatchToolEntry, BatchToolStatus, CodeGraphRow, CodeGraphSource, GrepFileEntry, INDEX_TRUNCATED,
    IndexDirectoryEntryKind, IndexLine, IndexLineSemantic, IndexOutput, IndexSourceRange,
    InstructionBlock, PatchedFile, SearchCap, SubagentProgress, ToolInput, ToolOutput,
    format_live_duration, format_settled_duration,
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
/// What a child's body clears past the trunk it hangs from, so its text lands
/// under the label the connector and sigil pushed across.
const BATCH_BODY_PAD: &str = "  ";
const BATCH_CHILD_INDENT_WIDTH: u16 = (TREE_GAP.len() + BATCH_BODY_PAD.len()) as u16;
const ANSWER_MARK: &str = "  \u{2713} ";
const ANSWER_INDENT: &str = "    ";
const NO_ANSWER: &str = "(no answer)";
/// Indented past the child's own sigil, so the activity reads as belonging to
/// the row above it rather than as another entry in the roster.
const CHILD_ACTIVITY_SEPARATOR: &str = " · ";
/// Says a child is folded, so a row with nothing under it is not mistaken for
/// one whose body was hidden.
const BATCH_FOLDED_MARK: &str = " \u{2026}";
const QUEUED_ANNOTATION: &str = "queued";
/// What the markdown renderer calls a width it should not wrap to.
const UNCONSTRAINED_WIDTH: u16 = 0;
const GREP_COUNT_SEP: &str = " \u{b7} ";
/// Workcell cuts a receipt at a byte bound and reports no line count for what
/// it dropped, so this says that the patch is short without inventing a number.
const PATCH_TRUNCATED: &str = "\u{2026} patch shortened";
/// A diff is already only the part that changed, so abridging one costs the
/// thing the card is for. This is the point past which a change stops being
/// something to read and starts being a file, and the card is held to it. Used
/// as a floor under the budget rather than in place of it, so a reader who
/// raises `ui.tool_output_lines.write` past it still gets what they asked for.
const DIFF_CARD_LINES: usize = 60;
const GREP_SUMMARY_INDENT: &str = "  ";
/// Past this many lines in one hunk, diffing its two sides again costs more
/// than the grouping it buys, so the wire's own order is drawn instead. A card
/// is rebuilt on every resize and theme change, so that cost is paid again
/// each time.
const MAX_REDIFF_LINES: usize = 4096;
/// The columns a diff body needs before a second gutter of line numbers is
/// worth what it takes away from the code.
const MIN_CODE_COLUMNS: usize = 60;
/// Below this a row has no room left to say anything, and breaking it would
/// cost more rows than the overflow it was meant to spare.
const MIN_WRAP_COLUMNS: usize = 8;
/// The characters a row's gutter is built from: the card's own indent, the four
/// tree glyphs, and the check a settled row opens with.
const GUTTER_CHARS: &str = " \u{2502}\u{251c}\u{2514}\u{2500}\u{2713}";
const MARKER_OPEN: char = '[';
const MARKER_CLOSE: char = ']';
const MARK_UNCHANGED: char = ' ';
const MARK_REMOVED: char = '-';
const MARK_ADDED: char = '+';
/// A line the alignment matched whose whitespace moved. Neither side of it is
/// new, so neither `-` nor `+` describes it.
const MARK_REINDENTED: char = '~';
/// Parts a card's source blocks by the blank row drawn between them, so a
/// selection spanning both copies the gap it saw rather than closing it.
const BLOCK_GAP: &str = "\n\n";
/// One level under the heading a copied tool card opens with, so a batch's
/// children read as its sections rather than as cards of their own.
const CHILD_HEADING_LEVEL: &str = "###";

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

pub(super) fn truncation_line(truncated: usize) -> Line<'static> {
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
    /// The runs of rows holding source code, and what to call each, so a copy
    /// that ran past one can fence it rather than drop code into prose. A batch
    /// card holds one per child, which is why this is not a single block.
    pub code: Vec<CodeBlock>,
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

    /// Names the code blocks after the language a tool declared for them.
    fn named(mut self, language: &str) -> Self {
        for code in &mut self.code {
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
    code: Vec<CodeBlock>,
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
        self.code.extend(body.code.into_iter().map(|mut code| {
            code.rows.start += start;
            code.rows.end += start;
            code
        }));
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
    width: u16,
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
        code: Vec::new(),
    };
    let mut at = 0u32;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(shown.len());
    // A number names a line of the file, so a line too long for the card keeps
    // one number and hangs the rest of itself under the code it belongs to.
    let hang = " ".repeat(w + 1);
    for (i, text) in shown.iter().enumerate() {
        let nr = start_line + i;
        let mut spans = vec![gutter(&format!("{nr:>w$}"))];
        match &mut hl {
            Some(h) => spans.extend(highlight_spans(h, text)),
            None => spans.push(fallback_span(text)),
        }
        let row = code_row(&spans, text, at);
        at += text.len() as u32 + 1;
        let broken = wrap_styled(spans, 1, &hang, width);
        source.rows.extend(wrapped_provenance(&broken, &row));
        lines.extend(wrapped_lines(broken));
    }
    source.code.push(CodeBlock {
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
            // The marker is its own span so a break hangs the text under
            // itself rather than restarting it against the card's edge.
            Line::from(Vec::from([
                Span::styled(format!("{} ", item.status.marker()), style),
                Span::styled(item.content.clone(), style),
            ]))
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
            lines.push(Line::from(Vec::from([
                Span::styled(ANSWER_INDENT, t.tool_dim),
                Span::styled(NO_ANSWER, t.tool_dim),
            ])));
            continue;
        }
        for picked in &answer.labels {
            for (row, piece) in picked.lines().enumerate() {
                let prefix = if row == 0 { ANSWER_MARK } else { ANSWER_INDENT };
                lines.push(Line::from(Vec::from([
                    Span::styled(prefix, t.todo_completed),
                    Span::styled(piece.to_owned(), t.todo_completed),
                ])));
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
/// The transcript separates a row that merely wraps, which this does not: a
/// break is applied to the whole card at the end, once, and a child that takes
/// two rows is still one entry in this list. It reads as the list anyway,
/// because every child opens on its sigil and a continuation row opens on the
/// gutter it hangs under, which is the distinction the blank row was buying.
/// Spacing them here by what they might break into would put the gaps back
/// exactly where they are worst, since a long search header is what wraps.
fn render_batch(entries: &[BatchToolEntry], highlight: bool, limits: &RenderLimits) -> BatchCard {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut spans_out = Vec::new();
    let mut trace = SourceTrace::default();
    let mut previous_has_body = false;
    for (index, entry) in entries.iter().enumerate() {
        let view = limits.child(index, entry);
        // Resolved before the summary row so the separator below knows whether
        // this child is a list entry or a block.
        let body = view.map(|child| child_body(entry, highlight, &child, limits.live.get(&index)));
        let has_body = body.as_ref().is_some_and(|body| !body.lines.is_empty());
        // Every row of a child answers for it, whether or not a click would
        // change what is drawn: this is also how a dispatched child's rows are
        // traced back to the subagent they belong to.
        let target = Some(RowTarget(index));
        // The separator sits above a child that exists, so the trunk always has
        // somewhere left to go: drawn blank it would cut the tree in two at
        // every body.
        if !lines.is_empty() && (previous_has_body || has_body) {
            lines.push(Line::from(Span::styled(TREE_TRUNK.trim_end(), t.tool_dim)));
            rows.push(None);
        }
        previous_has_body = has_body;
        // A child is a node of the card's tree: the connector says whether any
        // sibling follows, and the trunk below it says the same thing for every
        // row this child owns.
        let last = index + 1 == entries.len();
        let (connector, continuation) = match last {
            true => (TREE_LAST, TREE_GAP),
            false => (TREE_BRANCH, TREE_TRUNK),
        };
        let tense = entry.status.into();
        let (sigil, label) = compact_sigil_label(&entry.tool, tense);
        let inflected = inflected_header(&entry.tool, &entry.summary, tense);
        // Once the body carries the script, the row keeps only what the body
        // does not say. The same trade a card's header makes, and for the same
        // reason: the body's copy is the numbered, highlighted one.
        let summary = match has_body && body_repeats_summary(entry) {
            true => "",
            false => &inflected,
        };
        let gap = if summary.is_empty() { "" } else { " " };
        let mut spans = vec![
            Span::styled(connector, t.tool_dim),
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
        if let Some(elapsed) = child_elapsed(entry, limits.started.get(&index)) {
            let clock = match entry.status {
                BatchToolStatus::Running => format_live_duration(elapsed),
                _ => format_settled_duration(elapsed),
            };
            spans.push(Span::styled(
                format!("{CHILD_ACTIVITY_SEPARATOR}{clock}"),
                t.tool_dim,
            ));
        }
        if let Some(tally) = limits.progress.get(&index).and_then(settled_tally) {
            spans.push(Span::styled(
                format!("{CHILD_ACTIVITY_SEPARATOR}{tally}"),
                t.tool_dim,
            ));
        }
        if body.is_none() && holds_a_body(entry) {
            spans.push(Span::styled(BATCH_FOLDED_MARK, t.tool_dim));
        }
        // A child is a section of the card, not another run of its output, so
        // its summary row copies as the heading that says whose output follows.
        // Without it ten calls reach the clipboard as one undivided block.
        // A title too long for the card hangs under its own label, so the
        // connector and the sigil are said once and the tree stays legible.
        let broken = wrap_styled(
            spans,
            2,
            &format!("{continuation}{BATCH_BODY_PAD}"),
            limits.width,
        );
        let heading = child_heading(index, entry);
        trace.record(lines.len(), heading_rows(&broken, heading));
        rows.resize(rows.len() + broken.len(), target);
        lines.extend(wrapped_lines(broken));
        // A child's own output is content and clears the trunk; what the child
        // dispatched hangs off it as nodes. Content first, so reading down a
        // child never steps back out to a shallower level than the row above.
        if let Some(body) = body {
            if let Some(span) = body.span {
                spans_out.push(span.shifted(lines.len(), Some(index)));
            }
            rows.resize(rows.len() + body.lines.len(), target);
            match body.source {
                Some(source) => trace.record(lines.len(), source.indented()),
                None => trace.abandon(),
            }
            lines.extend(indent_all(body.lines, continuation));
        }
        if let Some(progress) = limits.progress.get(&index).filter(|p| p.is_live()) {
            let progress_lines = child_progress_lines(progress, continuation);
            rows.resize(rows.len() + progress_lines.len(), target);
            lines.extend(progress_lines);
        }
    }
    BatchCard {
        source: trace.finish(&lines),
        lines,
        rows,
        spans: spans_out,
    }
}

/// The heading a child's summary row copies as, numbered so a reader can match
/// a section against the roster they were looking at.
///
/// The row's own spans name nothing: the connector, the sigil and the tense are
/// the card's way of saying what the heading says in words, and copying the
/// glyphs would put box-drawing characters in the middle of a document.
fn child_heading(index: usize, entry: &BatchToolEntry) -> BodySource {
    let summary = entry.summary.trim();
    let gap = if summary.is_empty() { "" } else { " " };
    let text = format!(
        "{CHILD_HEADING_LEVEL} {}. `{}`{gap}{summary}",
        index + 1,
        entry.tool
    );
    BodySource {
        rows: Vec::from([LineProvenance {
            line: Some(0..text.len() as u32),
            spans: Vec::new(),
        }]),
        text,
        code: Vec::new(),
    }
}

/// The heading's rows, one per display row the title took. Only the first
/// names the heading text: the rest are the same title still being said, and a
/// selection that reaches them copies it once.
fn heading_rows(broken: &[Vec<SpanPiece>], heading: BodySource) -> BodySource {
    let mut rows = heading.rows;
    let first = rows.pop().unwrap_or_else(|| LineProvenance::chrome(0));
    for row in broken {
        rows.push(LineProvenance {
            line: first.line.clone(),
            spans: vec![SpanSource::Chrome; row.len()],
        });
    }
    BodySource {
        rows,
        text: heading.text,
        code: heading.code,
    }
}

/// What a batch card draws, with the rows and the source every line of it has
/// to be readable through: a click resolves a row to the child that owns it,
/// and a selection resolves it to the text that child answered with.
struct BatchCard {
    lines: Vec<Line<'static>>,
    rows: Vec<Option<RowTarget>>,
    spans: Vec<ScrollSpan>,
    source: Option<BodySource>,
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

/// A child's clock, on the same terms a standalone card keeps one: wall time
/// while it runs, and the command's own measured time once it lands.
///
/// `shell` alone, because `shell` alone reports a duration, and that one is
/// persisted with the output, so a restored roster reads as the run did. A
/// child that started before the roster was being timed has no clock rather
/// than a wrong one.
fn child_elapsed(entry: &BatchToolEntry, started: Option<&Instant>) -> Option<Duration> {
    if entry.status == BatchToolStatus::Running {
        return started
            .filter(|_| names_tool(SHELL_TOOL_NAME, &entry.tool))
            .map(|started| started.elapsed());
    }
    match entry.output.as_ref() {
        Some(ToolOutput::Shell(output)) => Some(Duration::from_millis(output.duration_ms)),
        _ => None,
    }
}

/// What a settled child's dispatch is still worth saying. Its output already
/// carries the result, so only the tally survives, and it rides the child's
/// own row: hung off it as a node it would be a branch of the tree holding
/// nothing but a clock, with the child's body drawn to the left of it.
fn settled_tally(progress: &ToolProgress) -> Option<String> {
    match progress.is_live() {
        true => None,
        false => Some(SubagentProgress::tally(
            progress.report.tools,
            progress.elapsed(),
        )),
    }
}

/// How a dispatched child is getting on, and the roster it is working through
/// when the tool it is running is itself a batch.
///
/// `continuation` is the trunk carried down from the child that owns this
/// progress: `│   ` while that child has siblings below it, spaces once it is
/// the last one.
fn child_progress_lines(progress: &ToolProgress, continuation: &str) -> Vec<Line<'static>> {
    let theme = theme::current();
    let mut spans = vec![Span::styled(
        format!("{continuation}{TREE_LAST}"),
        theme.tool_dim,
    )];
    if let Some(sigil) = activity_sigil(&progress.report.activity) {
        spans.push(Span::styled(format!("{sigil} "), theme.tool_prefix));
    }
    spans.push(Span::styled(
        activity_label(&progress.report.activity),
        theme.tool_prefix,
    ));
    if let Some(detail) = activity_detail(&progress.report.activity) {
        spans.push(Span::styled(format!(" {detail}"), theme.tool_dim));
    }
    spans.push(Span::styled(
        format!(
            "{CHILD_ACTIVITY_SEPARATOR}{}",
            SubagentProgress::tally(progress.report.tools, progress.elapsed())
        ),
        theme.tool_dim,
    ));
    let mut lines = vec![Line::from(spans)];
    // The activity row is the last node under its child, so everything below
    // it clears the trunk rather than continuing one.
    let children = progress.report.activity.children();
    for (index, child) in children.iter().enumerate() {
        let connector = match index + 1 == children.len() {
            true => TREE_LAST,
            false => TREE_BRANCH,
        };
        lines.push(Line::from(activity_child_spans(
            child,
            format!("{continuation}{TREE_GAP}{connector}"),
        )));
    }
    lines
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
) -> ChildBody {
    let output = entry.output.as_ref();
    // Every answer that is text goes through one place, so no arm can be the
    // one that forgets the script. `render_tool_content` draws the script
    // itself, which is why structured output is the exception here rather than
    // a case alongside the others.
    let text = if entry.status == BatchToolStatus::Error {
        Some(plain_body(
            &output.map_or(String::new(), ToolOutput::as_text),
            limits.width,
        ))
    } else if let Some(tail) = live.filter(|text| output.is_none() && !text.is_empty()) {
        Some(plain_body(tail, limits.width))
    } else {
        match output {
            Some(ToolOutput::Markdown(text)) => Some(markdown_body(&text.text, limits.width)),
            Some(ToolOutput::Plain(text) | ToolOutput::ReadDir(text)) => {
                Some(plain_body(&text.text, limits.width))
            }
            Some(ToolOutput::Shell(shell)) => Some(plain_body(&shell.raw_text(), limits.width)),
            _ => None,
        }
    };
    match text {
        Some((lines, source)) => {
            // A child that has answered has no tail left to chase, so its
            // footer is told to report where the window sits and nothing more.
            let tail = match entry.output.is_none() {
                true => ScrollTail::Live,
                false => ScrollTail::Settled,
            };
            let body = child_view(ChildBody::traced(lines, source), limits, tail);
            with_script(entry, highlight, limits, body)
        }
        None => {
            let content =
                render_tool_content(entry.input.as_ref(), output, highlight, limits.clone());
            ChildBody {
                lines: content.lines,
                source: content.source,
                truncation: content.truncation,
                span: None,
            }
        }
    }
}

/// A child's body with everything that has to stay parallel to its lines.
///
/// The rows travel with the lines because a window, a budget and a script each
/// change the line count between here and the card: a row one of them left
/// behind points a copy at text that was never drawn.
struct ChildBody {
    lines: Vec<Line<'static>>,
    /// `None` where a renderer in the body named no source at all, which hands
    /// the card it lands in back to the scraping fallback.
    source: Option<BodySource>,
    truncation: bool,
    span: Option<ScrollSpan>,
}

impl ChildBody {
    fn traced(lines: Vec<Line<'static>>, source: BodySource) -> Self {
        Self {
            lines,
            source: Some(source),
            truncation: false,
            span: None,
        }
    }

    /// Keeps the rows behind the lines a window or a budget kept, giving the
    /// body up when they can no longer be told to line up. The blocks move with
    /// them, or a fence would land around rows the window never drew.
    fn keep_rows(&mut self, kept: Range<usize>) {
        self.source = self.source.take().and_then(|mut source| {
            source.rows = source.rows.get(kept.clone())?.to_vec();
            source.code = source
                .code
                .into_iter()
                .filter_map(|block| {
                    let start = block.rows.start.max(kept.start);
                    let end = block.rows.end.min(kept.end);
                    (start < end).then(|| CodeBlock {
                        rows: start - kept.start..end - kept.start,
                        ..block
                    })
                })
                .collect();
            Some(source)
        });
    }

    /// Marks a line the body was closed with, which is chrome wherever it came
    /// from: a scroll footer or a truncation notice.
    fn push_chrome(&mut self) {
        if let Some(source) = self.source.as_mut() {
            source.push_chrome(1);
        }
    }

    /// Holds the body to the budget its own card would hold it to, and says how
    /// much that hid.
    fn capped(mut self, budget: usize) -> Self {
        let (lines, truncated) = capped(std::mem::take(&mut self.lines), budget);
        self.lines = lines;
        if truncated {
            self.keep_rows(0..budget);
            self.push_chrome();
        }
        self.truncation |= truncated;
        self
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
    body: ChildBody,
) -> ChildBody {
    if entry.input.is_none() {
        return body;
    }
    let script = render_tool_content(entry.input.as_ref(), None, highlight, limits.clone());
    let mut lines = script.lines;
    if !lines.is_empty() && !body.lines.is_empty() {
        lines.push(Line::default());
    }
    let shift = lines.len();
    lines.extend(body.lines);
    // The two halves were painted from different texts, so their ranges are
    // rebased onto the one the card ends up holding rather than spliced.
    let mut trace = SourceTrace::default();
    for (start, source) in [(0, script.source), (shift, body.source)] {
        match source {
            Some(source) => trace.record(start, source),
            None => trace.abandon(),
        }
    }
    ChildBody {
        source: trace.finish(&lines),
        lines,
        truncation: body.truncation,
        span: body.span.map(|span| span.shift_lines(shift)),
    }
}

/// Holds a child to its window when it scrolls and to its budget otherwise.
///
/// The window takes the footer the budget's notice would have taken. Both say
/// what is not being shown; a window says it as two edges and which one the
/// reader is pinned to, because that is what tells them whether output is
/// still arriving under what they are reading.
fn child_view(mut body: ChildBody, limits: &RenderLimits, tail: ScrollTail) -> ChildBody {
    let Some(window) = limits.scroll else {
        return body.capped(limits.budget);
    };
    let total = body.lines.len();
    let (start, end) = window.range(total);
    body.lines = body.lines[start..end].to_vec();
    body.keep_rows(start..end);
    let span = ScrollSpan {
        child: None,
        first: 0,
        lines: body.lines.len(),
        total,
        offset: start,
    };
    let Some(footer) = scroll_footer_text(start, total - end, tail) else {
        return body;
    };
    body.lines
        .push(Line::from(Span::styled(footer, theme::current().tool_dim)));
    body.push_chrome();
    body.truncation = true;
    body.span = Some(span);
    body
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
/// matters.
///
/// The width rides on the limits, which is what reaches the highlight worker,
/// and `HighlightKey` carries it so a resize re-renders instead of splicing
/// back an answer broken for the width the terminal used to be. Zero is the
/// renderer's own word for not wrapping, and stays what a caller with no width
/// to give gets.
///
/// Breaking a paragraph here does not put a newline on the clipboard: the rows
/// of one source line all name that line, so copy reads it back as it was
/// written rather than as it was drawn.
fn markdown_body(text: &str, width: u16) -> (Vec<Line<'static>>, BodySource) {
    let (painted, parsed) = text_to_wrapped(
        text,
        theme::current().assistant,
        width,
        caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES,
    );
    (
        painted.lines,
        BodySource {
            text: parsed.to_string(),
            rows: painted.provenance,
            code: Vec::new(),
        },
    )
}

/// Text with no markdown behind it, broken where ratatui would break it for
/// the same reason a markdown body is: a row the terminal wraps for us
/// restarts at column zero and leaves the tree behind.
///
/// Every row of one source line names that whole line, so a selection reaching
/// across a break copies the line unbroken.
///
/// The whole body is one block, because it is text a tool printed rather than
/// prose: fenced where it lands, its line breaks survive a copy into markdown
/// instead of reflowing into a paragraph.
pub(super) fn plain_body(text: &str, width: u16) -> (Vec<Line<'static>>, BodySource) {
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut at = 0u32;
    for source in text.lines() {
        let line = at..at + source.len() as u32;
        at = line.end + 1;
        let ranges = wrapped_ranges(source, width);
        let last = ranges.len() - 1;
        for (index, range) in ranges.into_iter().enumerate() {
            // The break swallowed the spaces the row ends on, and drawing them
            // would push it back past the width it was broken to.
            let drawn = match index == last {
                true => &source[range.clone()],
                false => source[range.clone()].trim_end_matches(' '),
            };
            lines.push(Line::from(drawn.to_owned()));
            rows.push(LineProvenance {
                line: Some(line.clone()),
                spans: vec![SpanSource::Range(Source::verbatim(
                    line.start + range.start as u32..line.start + range.end as u32,
                ))],
            });
        }
    }
    let code = Vec::from([CodeBlock {
        rows: 0..rows.len(),
        language: None,
    }]);
    (
        lines,
        BodySource {
            text: text.to_owned(),
            rows,
            code,
        },
    )
}

/// One piece of a styled row, on the display row the break put it on.
///
/// `origin` names the span it was cut from, or `None` for the gutter a
/// continuation was given, which stands for nothing the row said.
#[derive(Clone)]
struct SpanPiece {
    origin: Option<usize>,
    offset: usize,
    /// The bytes of the origin span this row stands for, which is not always
    /// what it draws: a break swallows the spaces it broke on, and drawing
    /// them would push the row back past the width it was broken to.
    len: usize,
    span: Span<'static>,
}

/// Breaks a styled row to `width`, keeping its gutter in the left column of
/// every display row it takes.
///
/// `gutter` counts the leading spans that are chrome — a connector, a line
/// number, a sigil. They stay on the first row; the rows after it get
/// `continuation` in their place, which must be the same width or the text
/// under it stops lining up. Left to ratatui, those rows would start at column
/// zero and the card's own structure would end at the first line too long for
/// it.
fn wrap_styled(
    spans: Vec<Span<'static>>,
    gutter: usize,
    continuation: &str,
    width: u16,
) -> Vec<Vec<SpanPiece>> {
    let gutter_width: usize = spans
        .iter()
        .take(gutter)
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let room = usize::from(width).saturating_sub(gutter_width);
    let content: String = spans
        .iter()
        .skip(gutter)
        .map(|span| span.content.as_ref())
        .collect();
    let unbroken = width == UNCONSTRAINED_WIDTH
        || room < MIN_WRAP_COLUMNS
        || UnicodeWidthStr::width(content.as_str()) <= room;
    let head: Vec<SpanPiece> = spans
        .iter()
        .take(gutter)
        .enumerate()
        .map(|(origin, span)| SpanPiece {
            origin: Some(origin),
            offset: 0,
            len: span.content.len(),
            span: span.clone(),
        })
        .collect();
    if unbroken {
        let tail = spans
            .into_iter()
            .enumerate()
            .skip(gutter)
            .map(|(origin, span)| SpanPiece {
                origin: Some(origin),
                offset: 0,
                len: span.content.len(),
                span,
            });
        return Vec::from([head.into_iter().chain(tail).collect()]);
    }
    let mut rows = Vec::new();
    let mut row = head;
    let mut cut = wrapped_ranges(&content, room as u16).into_iter().skip(1);
    let mut next = cut.next().map(|range| range.start);
    let mut at = 0usize;
    for (origin, span) in spans.into_iter().enumerate().skip(gutter) {
        let text = span.content.as_ref();
        let mut taken = 0usize;
        while let Some(boundary) = next.filter(|boundary| *boundary < at + text.len()) {
            let end = boundary - at;
            let whole = &text[taken..end];
            row.push(piece(
                origin,
                taken,
                whole,
                whole.trim_end_matches(' '),
                &span,
            ));
            rows.push(std::mem::take(&mut row));
            row.push(SpanPiece {
                origin: None,
                offset: 0,
                len: 0,
                span: Span::styled(continuation.to_owned(), theme::current().tool_dim),
            });
            taken = end;
            next = cut.next().map(|range| range.start);
        }
        let rest = &text[taken..];
        row.push(piece(origin, taken, rest, rest, &span));
        at += text.len();
    }
    rows.push(row);
    rows
}

/// A span narrowed to the part of it one display row stands for. `drawn` is
/// what reaches the screen, which drops the spaces a break swallowed; the
/// piece still answers for them so a copy reads back the line as written.
fn piece(origin: usize, offset: usize, text: &str, drawn: &str, span: &Span<'static>) -> SpanPiece {
    SpanPiece {
        origin: Some(origin),
        offset,
        len: text.len(),
        span: Span::styled(drawn.to_owned(), span.style),
    }
}

/// The rows `wrap_styled` produced, as lines.
fn wrapped_lines(rows: Vec<Vec<SpanPiece>>) -> Vec<Line<'static>> {
    rows.into_iter()
        .map(|row| Line::from(row.into_iter().map(|piece| piece.span).collect::<Vec<_>>()))
        .collect()
}

/// Whether a span standing at the head of a row is chrome rather than content.
///
/// The question is only ever asked of a whole span, and the span boundary was
/// drawn by the renderer: a line number, a tree connector, a todo marker and
/// the card's own indent are each already spans of their own. That is what
/// makes this a reading of the row's structure rather than a guess about its
/// text.
fn is_gutter(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let bare = text.trim_end_matches(' ');
    bare.starts_with(MARKER_OPEN) && bare.ends_with(MARKER_CLOSE)
        || text
            .chars()
            .all(|c| GUTTER_CHARS.contains(c) || c.is_ascii_digit())
}

/// The leading spans of a row that are its gutter.
///
/// Never the whole row: a row that is nothing but chrome has no text to hang,
/// and claiming all of it would leave the break no room to put anything in.
fn gutter_spans(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .take_while(|span| is_gutter(span.content.as_ref()))
        .count()
        .min(spans.len().saturating_sub(1))
}

/// One row broken to `width`, hung under whatever gutter it opens with.
fn wrap_row(spans: Vec<Span<'static>>, width: u16) -> Vec<Vec<SpanPiece>> {
    let gutter = gutter_spans(&spans);
    let pad = " ".repeat(
        spans
            .iter()
            .take(gutter)
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum(),
    );
    wrap_styled(spans, gutter, &pad, width)
}

/// A card's lines broken to a width, holding on to which rows each line became
/// so that everything the card indexed by line can be moved onto them.
///
/// Wrapping is the last thing that happens to a card, and it happens to every
/// card in one place. A renderer that draws a gutter therefore does not have to
/// know the width it will be drawn at, which is the assumption the rest of this
/// module is written under.
pub(crate) struct WrappedRows {
    per_line: Vec<Vec<Vec<SpanPiece>>>,
}

impl WrappedRows {
    pub(crate) fn new(lines: Vec<Line<'static>>, width: u16) -> Self {
        Self {
            per_line: lines
                .into_iter()
                .map(|line| wrap_row(line.spans, width))
                .collect(),
        }
    }

    /// The first display row of each source line, with the total appended so a
    /// range over lines maps to a range over rows by its endpoints alone.
    fn starts(&self) -> Vec<usize> {
        let mut at = 0;
        let mut starts = Vec::with_capacity(self.per_line.len() + 1);
        for rows in &self.per_line {
            starts.push(at);
            at += rows.len();
        }
        starts.push(at);
        starts
    }

    pub(crate) fn row_of(&self, line: usize) -> usize {
        self.starts()[line.min(self.per_line.len())]
    }

    pub(crate) fn lines(&self) -> Vec<Line<'static>> {
        self.per_line
            .iter()
            .flat_map(|rows| wrapped_lines(rows.to_vec()))
            .collect()
    }

    /// A value held per line, moved onto every row that line became.
    pub(crate) fn expand<T: Clone>(&self, per_line: Vec<T>) -> Vec<T> {
        per_line
            .into_iter()
            .zip(&self.per_line)
            .flat_map(|(value, rows)| std::iter::repeat_n(value, rows.len()))
            .collect()
    }

    /// Provenance for every row, narrowed to the bytes each one drew.
    pub(crate) fn provenance(&self, rows: Vec<LineProvenance>) -> Vec<LineProvenance> {
        rows.into_iter()
            .zip(&self.per_line)
            .flat_map(|(source, rows)| wrapped_provenance(rows, &source))
            .collect()
    }

    /// A range of lines as the range of rows they became.
    pub(crate) fn range(&self, range: Range<usize>) -> Range<usize> {
        self.row_of(range.start)..self.row_of(range.end)
    }

    /// A card's source, moved onto the rows the break produced. The text is
    /// what was written, so it never moves.
    pub(crate) fn body(&self, source: BodySource) -> BodySource {
        BodySource {
            rows: self.provenance(source.rows),
            code: source
                .code
                .into_iter()
                .map(|block| CodeBlock {
                    rows: self.range(block.rows),
                    ..block
                })
                .collect(),
            text: source.text,
        }
    }

    /// Where a span of a line ended up, as the row holding it and its index in
    /// that row. A span the break dropped entirely keeps the line's first row.
    pub(crate) fn span_at(&self, line: usize, span: usize) -> (usize, usize) {
        let first = self.row_of(line);
        let Some(rows) = self.per_line.get(line) else {
            return (first, span);
        };
        for (offset, row) in rows.iter().enumerate() {
            if let Some(at) = row.iter().position(|piece| piece.origin == Some(span)) {
                return (first + offset, at);
            }
        }
        (first, span)
    }

    /// A per-span value held for one line, rebuilt for each row that line
    /// became, so a row answers for exactly the spans it drew.
    pub(crate) fn spans_of<T: Clone + Default>(&self, line: usize, values: &[T]) -> Vec<Vec<T>> {
        self.per_line
            .get(line)
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        row.iter()
                            .map(|piece| {
                                piece
                                    .origin
                                    .and_then(|origin| values.get(origin))
                                    .cloned()
                                    .unwrap_or_default()
                            })
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The provenance of every row a break produced, from the row's own.
///
/// Each row names the whole source line, so a selection reaching across a
/// break copies it once and unbroken. A span the break cut is narrowed to the
/// bytes that landed on each row, and one drawn from text that does not
/// survive the trip to the screen stays atomic, because its offsets no longer
/// count its bytes.
fn wrapped_provenance(rows: &[Vec<SpanPiece>], source: &LineProvenance) -> Vec<LineProvenance> {
    rows.iter()
        .map(|row| LineProvenance {
            line: source.line.clone(),
            spans: row
                .iter()
                .map(
                    |piece| match piece.origin.and_then(|origin| source.spans.get(origin)) {
                        Some(SpanSource::Range(range)) => {
                            SpanSource::Range(narrowed(range, piece.offset, piece.len))
                        }
                        // The gutter a continuation was given stands for nothing
                        // the row said; every other span keeps what it had.
                        Some(other) => other.clone(),
                        None => SpanSource::Chrome,
                    },
                )
                .collect(),
        })
        .collect()
}

/// The part of a span's source one row drew. A verbatim range counts bytes, so
/// it can be cut; anything else stands for the whole span however much of it
/// is on screen.
fn narrowed(source: &Source, offset: usize, len: usize) -> Source {
    let span = (source.range.end - source.range.start) as usize;
    match source.verbatim && offset + len <= span {
        true => {
            let start = source.range.start + offset as u32;
            Source::verbatim(start..start + len as u32)
        }
        false => source.clone(),
    }
}

/// The bytes of each display row `line` occupies at `width`.
///
/// The ranges tile the line: `wrap_breaks` reports where a row starts, having
/// skipped the spaces it broke on, so the row before a break absorbs them and
/// the ranges still read back as the line that was written.
fn wrapped_ranges(line: &str, width: u16) -> Vec<Range<usize>> {
    if width == UNCONSTRAINED_WIDTH || UnicodeWidthStr::width(line) <= usize::from(width) {
        return iter::once(0..line.len()).collect();
    }
    let chars: Vec<char> = line.chars().collect();
    let mut offsets: Vec<usize> = line.char_indices().map(|(at, _)| at).collect();
    offsets.push(line.len());
    let mut starts = vec![0usize];
    starts.extend(wrap_breaks(&chars, width).into_iter().map(|b| b.start));
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(chars.len());
            offsets[start]..offsets[end]
        })
        .collect()
}

/// Indents a child's body to sit under its label, carrying the trunk down the
/// left of every row so the body reads as hanging off the connector above it
/// rather than floating between two siblings.
fn indent_all(lines: Vec<Line<'static>>, continuation: &str) -> Vec<Line<'static>> {
    let indent = format!("{continuation}{BATCH_BODY_PAD}");
    let style = theme::current().tool_dim;
    lines
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::styled(indent.clone(), style));
            line
        })
        .collect()
}

/// Each file gets its own heading, because a patch that touches three files
/// is otherwise three diffs with nothing saying where one ends.
///
/// A patch that touches one is the exception. There is no boundary to mark,
/// and the card header already names that file and carries the same counts, so
/// the heading would be the row above it spelled a second time.
fn render_patch(files: &[PatchedFile], width: u16) -> Vec<Line<'static>> {
    let theme = theme::current();
    let needs_headings = files.len() > 1;
    let mut lines = Vec::new();
    for file in files {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        if needs_headings {
            lines.push(Line::from(vec![
                Span::styled(file.path.clone(), theme.tool_prefix),
                Span::styled(
                    format!(" +{} -{}", file.additions, file.deletions),
                    theme.tool_annotation,
                ),
            ]));
        }
        lines.extend(render_unified_patch(&file.patch, width));
        if file.truncated {
            lines.push(Line::from(Span::styled(
                PATCH_TRUNCATED.to_owned(),
                theme.tool_dim,
            )));
        }
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
pub(crate) fn render_live_body(body: &str, width: u16) -> Vec<Line<'static>> {
    let shown: Vec<String> = body.lines().map(String::from).collect();
    render_code(None, 1, &shown, shown.len(), usize::MAX, width).lines
}

/// How many rows the full rendering would take. Counted rather than rendered,
/// so deciding to condense never costs the highlighting of results nobody is
/// going to see.
fn grep_height(entries: &[GrepFileEntry]) -> usize {
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
            1 + separators + lines
        })
        .sum()
}

/// A notice costs the row it saves, so hiding exactly one of anything is
/// never worth it. Answers with how many to show and how many that hides.
pub(super) fn within(total: usize, room: usize) -> (usize, usize) {
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

        out.push(Line::from(Span::styled(
            entry.path.clone(),
            theme::current().tool_path,
        )));
        budget -= 1;

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
    let height = grep_height(entries);
    if height <= max_lines {
        return (render_grep_lines(entries, height, highlight), false);
    }
    if entries.len() > 1 {
        return (render_grep_summary(entries, max_lines), true);
    }

    // One file has no distribution to summarise, so it names itself and the
    // matches take what room is left. The count is left to the card header.
    // The name takes its row before the matches are budgeted, because a match
    // the card cannot place is one the reader cannot act on.
    let (shown, hidden) = within(height.saturating_sub(1), max_lines.saturating_sub(2));
    let mut out = render_grep_lines(entries, shown + 1, highlight);
    out.push(Line::from(Span::styled(
        expand_notice(&format!("{hidden} lines")),
        theme::current().tool_dim,
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
/// What a graph row's continuations clear, so they hang under the name rather
/// than under the sigil that opened the row.
const GRAPH_HANG: &str = "  ";
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
    width: u16,
) -> (Vec<Line<'static>>, bool) {
    let theme = theme::current();
    // The headline and footer are the two lines that make the rest
    // interpretable, so they are never what gets dropped.
    let room = max_lines.saturating_sub(2);
    let body_height = rows.len() + source.map_or(0, |source| source.lines.len() + 1);
    let (shown, hidden) = within(body_height, room);
    let truncated = hidden > 0;

    let mut lines = wrapped_lines(wrap_styled(
        Vec::from([Span::styled(headline.to_owned(), theme.tool_dim)]),
        0,
        GRAPH_HANG,
        width,
    ));

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
        lines.extend(wrapped_lines(wrap_styled(
            code_graph_row(row, name_width, hop_width).spans,
            1,
            GRAPH_HANG,
            width,
        )));
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
                width,
            );
            lines.extend(body.lines);
        }
    }

    if truncated {
        lines.push(truncation_line(hidden));
    }
    let caveated = GRAPH_CAVEATS.iter().any(|caveat| footer.contains(caveat));
    let footer_style = match caveated {
        true => theme.error,
        false => theme.tool_dim,
    };
    lines.extend(wrapped_lines(wrap_styled(
        Vec::from([Span::styled(footer.to_owned(), footer_style)]),
        0,
        GRAPH_HANG,
        width,
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
    width: u16,
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
        let rendered = render_code(hl, 1, &code_lines, total, remaining, width);
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

/// When each still-running child of one batch started, by index in the roster.
/// Only the children that report a duration of their own are in it, because
/// only those draw a clock worth counting up.
pub type ChildStarted = Arc<HashMap<usize, Instant>>;

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
    /// Whether the reader asked for the whole transcript open. A card small
    /// enough to draw entire answers to it the way it answers a click.
    pub expanded: bool,
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
    pub started: ChildStarted,
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
            started: ChildStarted::default(),
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

    pub fn with_progress(
        self,
        progress: ChildProgress,
        live: ChildLive,
        started: ChildStarted,
    ) -> Self {
        Self {
            progress,
            live,
            started,
            ..self
        }
    }

    pub fn with_width(self, width: u16) -> Self {
        Self { width, ..self }
    }

    /// Whether any child is still moving: a report redrawn every tick, a
    /// stream arriving as often as the command prints, or a clock counting up.
    /// A batch holding any of them renders here instead of being sent out and
    /// spliced back stale.
    ///
    /// None of them is in the worker's cache key, and none usefully could be:
    /// they all move on every frame. Without this a streaming child shows the
    /// first window that reached the worker and then freezes there until the
    /// call settles, and a running child's clock freezes with it.
    pub fn has_live_rows(&self) -> bool {
        !self.live.is_empty()
            || !self.started.is_empty()
            || self.progress.values().any(ToolProgress::is_live)
    }

    pub fn is_expanded(&self) -> bool {
        self.budget == usize::MAX
    }

    /// What a card whose body is bounded by its own shape is drawn within. A
    /// reader who opened the whole transcript asked for this card too, and
    /// such a card has no unbounded body to protect them from.
    pub fn bounded_budget(&self) -> usize {
        if self.policy.expanded {
            usize::MAX
        } else {
            self.budget
        }
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
            started: ChildStarted::default(),
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

/// The start times of every batch that has a child still running a clock, by
/// parent tool id.
pub type BatchStartedMap = HashMap<String, ChildStarted>;

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
        let script = render_code(hl, 1, &code_lines, total, total, limits.width);
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
                limits.width,
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
                limits.width,
            );
            output_source = Some(code.source.named_for_path(path));
            (code.lines, code.truncated)
        }
        Some(ToolOutput::Diff {
            path,
            before,
            after,
            ..
        }) => capped(
            render_diff(
                highlight.then(|| caudra_highlight::syntax_for_path(path)),
                before,
                after,
                limits.width,
            ),
            limits.budget.max(DIFF_CARD_LINES),
        ),
        Some(ToolOutput::Patch { files }) => capped(
            render_patch(files, limits.width),
            limits.budget.max(DIFF_CARD_LINES),
        ),
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
            limits.width,
        ),
        Some(ToolOutput::Index(output @ IndexOutput::Directory { .. })) => {
            render_index_directory(output, limits.budget)
        }
        Some(ToolOutput::Instructions { blocks }) => {
            let mut instruction_lines = Vec::new();
            let trunc = render_instructions(
                blocks,
                &mut instruction_lines,
                limits.budget,
                highlight,
                limits.width,
            );
            (instruction_lines, trunc)
        }
        Some(ToolOutput::Environment {
            headline,
            summary,
            facts,
            commands,
        }) => environment_card::render(
            headline,
            summary,
            facts,
            commands,
            limits.bounded_budget(),
            limits.width,
        ),
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
            let card = render_batch(entries, highlight, &limits);
            output_rows = card.rows;
            output_spans = card.spans;
            output_source = card.source;
            (card.lines, false)
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
    let scroll_spans = output_spans
        .into_iter()
        .map(|span| span.shifted(body_start, span.child))
        .collect();
    wrapped_content(
        ToolContent {
            lines,
            rows,
            truncation,
            scroll_spans,
            source,
        },
        limits.width,
    )
}

/// The card, broken to the width it will be drawn at.
///
/// Everything above this point lays the card out in logical lines and leaves
/// the width to the end, so there is exactly one place where a gutter can be
/// lost to a break, and it is this one.
fn wrapped_content(content: ToolContent, width: u16) -> ToolContent {
    let wrapped = WrappedRows::new(content.lines, width);
    ToolContent {
        lines: wrapped.lines(),
        rows: wrapped.expand(content.rows),
        truncation: content.truncation,
        scroll_spans: content
            .scroll_spans
            .into_iter()
            .map(|span| ScrollSpan {
                first: wrapped.row_of(span.first),
                ..span
            })
            .collect(),
        source: content.source.map(|source| wrapped.body(source)),
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
    use caudra_agent::tools::{BATCH_TOOL_NAME, FILE_GREP_TOOL_NAME, ToolEffect};
    use caudra_agent::{
        ActivityChild, EnvironmentFact, GrepLine, GrepMatchGroup, ShellOutput, SubagentActivity,
        TextOutput,
    };
    use std::time::Duration;
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
            UNCONSTRAINED_WIDTH,
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

        let rendered = render_code(hl, 1, &code_lines, 1, usize::MAX, UNCONSTRAINED_WIDTH);

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

    const NUMBERED_ONCE: &str = "a source line is numbered once however many rows it takes, and \
        its continuations hang under the code rather than restarting at column zero";
    const CODE_CARD_WIDTH: u16 = 30;

    /// The reported bug: a long command in a numbered listing ran past the card
    /// and the terminal broke it, so the tail landed under the line numbers
    /// instead of under the code.
    #[test_case(1, 9 ; "one_digit")]
    #[test_case(97, 99 ; "two_digits")]
    #[test_case(998, 999 ; "three_digits")]
    fn a_numbered_line_is_numbered_once_however_many_rows_it_takes(start: usize, last: usize) {
        const LONG: &str = "git status --short; git diff --stat; git diff --cached --stat";
        let code_lines = Vec::from([LONG.to_owned(), "echo done".to_owned()]);
        let width = nr_width(last) + 1;

        let rendered = render_code(None, start, &code_lines, 2, usize::MAX, CODE_CARD_WIDTH);

        let drawn: Vec<String> = rendered
            .lines
            .iter()
            .map(|line| spans_text(&line.spans))
            .collect();
        assert!(drawn.len() > code_lines.len(), "{NUMBERED_ONCE}: {drawn:?}");
        let numbered: Vec<&String> = drawn
            .iter()
            .filter(|row| !row.starts_with(&" ".repeat(width)))
            .collect();
        assert_eq!(
            numbered.len(),
            code_lines.len(),
            "{NUMBERED_ONCE}: {drawn:?}"
        );
        assert!(
            drawn
                .iter()
                .all(|row| row.chars().count() <= usize::from(CODE_CARD_WIDTH)),
            "{NUMBERED_ONCE}: {drawn:?}"
        );
        assert_eq!(
            rendered.source.rows.len(),
            rendered.lines.len(),
            "{ROWS_PER_LINE}"
        );
    }

    /// A break must not put the clipboard out of step with the screen: the
    /// pieces of a cut span name the bytes each row drew, and together they are
    /// the line that was written.
    #[test]
    fn the_rows_a_numbered_line_broke_into_name_it_between_them() {
        const LONG: &str = "cargo nextest run --workspace --locked --no-fail-fast";
        let code_lines = Vec::from([LONG.to_owned()]);

        let rendered = render_code(None, 1, &code_lines, 1, usize::MAX, CODE_CARD_WIDTH);

        let copied: String = rendered
            .source
            .rows
            .iter()
            .flat_map(|row| &row.spans)
            .filter_map(|span| match span {
                SpanSource::Range(source) => Some(
                    &rendered.source.text[source.range.start as usize..source.range.end as usize],
                ),
                SpanSource::Chrome | SpanSource::Unknown => None,
            })
            .collect();
        assert_eq!(copied, LONG, "{NUMBERED_ONCE}");
        for (row, line) in rendered.source.rows.iter().zip(&rendered.lines) {
            assert_eq!(row.spans.len(), line.spans.len(), "{SPANS_PER_ROW}");
            assert_eq!(row.line, Some(0..LONG.len() as u32), "{NUMBERED_ONCE}");
        }
    }

    const PATCH: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -8,3 +8,4 @@\n context\n-gone\n+added\n+also added\n";
    const NUMBERED_MSG: &str = "context and removed lines carry their real file line number";
    const AFTER_GUTTER_MSG: &str = "an added line is numbered from the side it exists on";
    const HUNK_GAP_MSG: &str = "a jump between hunks must be marked, not silently closed";
    const HEADING_MSG: &str = "each file names itself and its size";
    const FIRST_PATH: &str = "src/lib.rs";
    const SECOND_PATH: &str = "src/main.rs";
    const LONE_HEADING_MSG: &str =
        "the card header already names a lone file, so the body must not name it again";
    const WIRE_HEADER_MSG: &str = "file headers belong to the wire format";
    const SHORTENED_MSG: &str = "a patch cut short must say so, and one that is whole must not";

    fn patch_text(files: &[PatchedFile]) -> Vec<String> {
        render_patch(files, UNCONSTRAINED_WIDTH)
            .iter()
            .map(line_text)
            .collect()
    }

    fn patched(path: &str, patch: &str) -> PatchedFile {
        PatchedFile {
            path: path.into(),
            patch: patch.into(),
            additions: 2,
            deletions: 1,
            truncated: false,
        }
    }

    fn one_file(patch: &str) -> Vec<PatchedFile> {
        vec![patched(FIRST_PATH, patch)]
    }

    fn two_files(patch: &str) -> Vec<PatchedFile> {
        vec![patched(FIRST_PATH, patch), patched(SECOND_PATH, patch)]
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

    /// Smaller than any real edit renders to, so a card that honoured it would
    /// be hiding the change it exists to show.
    const BUDGET_ROWS: usize = 2;
    const OVER_CEILING: usize = DIFF_CARD_LINES + 10;
    const WHOLE_MSG: &str = "a diff is already only the part that changed, so a card draws one \
                             whole however small the budget it was handed";
    const CEILING_MSG: &str =
        "past the ceiling a change has become a file, and the card holds it there and says so";
    const OPENED_MSG: &str = "an opened card draws the whole body it was withholding";

    fn tool_content(output: &ToolOutput, budget: usize) -> ToolContent {
        render_tool_content(
            None,
            Some(output),
            false,
            RenderLimits::new(false, budget, BatchViews::default(), TOOL_LINES),
        )
    }

    fn short_diff() -> ToolOutput {
        ToolOutput::Diff {
            path: FIRST_PATH.into(),
            before: "a\nb\nc\nd\ne\n".into(),
            after: "A\nB\nC\nD\nE\n".into(),
            summary: String::new(),
        }
    }

    fn long_diff() -> ToolOutput {
        let side = |tag: &str| (0..OVER_CEILING).map(|i| format!("{tag} {i}\n")).collect();
        ToolOutput::Diff {
            path: FIRST_PATH.into(),
            before: side("before"),
            after: side("after"),
            summary: String::new(),
        }
    }

    fn short_patch() -> ToolOutput {
        ToolOutput::Patch {
            files: two_files(PATCH),
        }
    }

    fn long_patch() -> ToolOutput {
        let mut patch =
            format!("--- a/{FIRST_PATH}\n+++ b/{FIRST_PATH}\n@@ -1 +1,{OVER_CEILING} @@\n");
        for i in 0..OVER_CEILING {
            patch.push_str(&format!("+added {i}\n"));
        }
        ToolOutput::Patch {
            files: one_file(&patch),
        }
    }

    /// The budget an edit shares with a write is seven rows, which is less than
    /// an edit of three lines draws, so holding a diff to it hid the change and
    /// charged a click for it. The ceiling is what a change stops being worth
    /// reading past, and it is a floor under the budget rather than a second
    /// bound: a card opened by hand still draws everything.
    #[test_case(short_diff, long_diff ; "a_diff")]
    #[test_case(short_patch, long_patch ; "a_patch")]
    fn an_edit_card_is_drawn_whole_up_to_its_ceiling(
        short: fn() -> ToolOutput,
        long: fn() -> ToolOutput,
    ) {
        let held = tool_content(&short(), BUDGET_ROWS);
        assert!(!held.truncation, "{WHOLE_MSG}");
        assert!(held.lines.len() > BUDGET_ROWS + 1, "{WHOLE_MSG}");

        let long = long();
        let cut = tool_content(&long, BUDGET_ROWS);
        assert!(cut.truncation, "{CEILING_MSG}");
        assert_eq!(cut.lines.len(), DIFF_CARD_LINES + 1, "{CEILING_MSG}");
        assert!(
            line_text(cut.lines.last().unwrap()).contains(EXPAND_AFFORDANCE),
            "{CEILING_MSG}"
        );

        let whole = tool_content(&long, usize::MAX);
        assert!(!whole.truncation, "{OPENED_MSG}");
        assert!(whole.lines.len() > DIFF_CARD_LINES + 1, "{OPENED_MSG}");
    }

    /// A shortened patch describes less than its counts claim, and a reader
    /// with no notice would take the part it shows for the whole change.
    #[test]
    fn a_shortened_patch_says_that_it_stops_early() {
        let mut files = one_file(PATCH);
        assert!(
            !patch_text(&files).iter().any(|row| row == PATCH_TRUNCATED),
            "{SHORTENED_MSG}"
        );

        files[0].truncated = true;
        let rendered = patch_text(&files);
        assert_eq!(
            rendered.last().map(String::as_str),
            Some(PATCH_TRUNCATED),
            "{SHORTENED_MSG}: {rendered:?}"
        );
    }

    /// The `---`/`+++` header names the file twice over, which the card header
    /// has already done, so it must not reach the transcript.
    #[test]
    fn a_patch_drops_the_file_header_lines() {
        let rendered = patch_text(&one_file(PATCH)).join("\n");
        assert!(
            !rendered.contains("+++") && !rendered.contains("--- a/"),
            "{WIRE_HEADER_MSG}: {rendered}"
        );
    }

    /// The card header already names a lone file and carries its counts, so a
    /// heading would be that row spelled a second time.
    #[test]
    fn a_lone_file_gets_no_heading() {
        let rendered = patch_text(&one_file(PATCH));
        assert!(
            !rendered.iter().any(|row| row.contains(FIRST_PATH)),
            "{LONE_HEADING_MSG}: {rendered:?}"
        );
    }

    /// Two diffs have to say where one ends, which is a job no card header can
    /// do once it has degraded to reporting a count of files.
    #[test]
    fn several_files_each_keep_their_heading() {
        let rendered = patch_text(&two_files(PATCH)).join("\n");
        for path in [FIRST_PATH, SECOND_PATH] {
            assert!(
                rendered.contains(&format!("{path} +2 -1")),
                "{HEADING_MSG}: {rendered}"
            );
        }
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

    /// Every row of a one-file patch is a diff row, its naming having been left
    /// to the card header.
    fn patch_rows(patch: &str) -> Vec<String> {
        patch_text(&one_file(patch))
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
    #[test_case(&[("a.rs", &[1_usize,2])],                              5, 3 ; "no_truncation_when_fits")]
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

    /// The reported bug: a grep whose matches all fit named line numbers and
    /// nothing else, and the card header names the directory the search was
    /// pointed at rather than the file that matched, so which file it was could
    /// not be recovered from the card at all.
    #[test]
    fn a_single_file_grep_that_fits_still_names_its_file() {
        let rendered = grep_text(&[("a.rs", &[1_usize, 2])], 10);
        assert_eq!(rendered[0], "a.rs");
        assert!(rendered[1].contains("code at a.rs:1"));
        assert!(!rendered.iter().any(|l| l.contains(EXPAND_AFFORDANCE)));
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

    /// A match row is its gutter and its text; a row naming a file is the name
    /// alone. Telling them apart by shape rather than by looking for a path
    /// keeps the check honest when the match text quotes a path itself.
    #[test]
    fn multi_file_grep_headers_and_alignment() {
        let entries = grep_entries(&[("a.rs", &[1]), ("b.rs", &[100])]);
        let (lines, _) = render_grep_results(&entries, None, 10, false);

        let named: Vec<String> = lines
            .iter()
            .filter(|line| line.spans.len() == 1)
            .map(line_text)
            .collect();
        assert_eq!(named, vec!["a.rs".to_owned(), "b.rs".to_owned()]);

        let gutters: Vec<usize> = lines
            .iter()
            .filter(|line| line.spans.len() > 1)
            .map(|line| line.spans[0].content.chars().count())
            .collect();
        assert_eq!(
            gutters,
            vec![nr_width(100) + 1; 2],
            "gutter widths should be uniform across files"
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
        let truncated =
            render_instructions(&blocks, &mut lines, max_lines, false, UNCONSTRAINED_WIDTH);
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
        let truncated = render_instructions(
            &blocks,
            &mut lines,
            MAX_INSTRUCTION_LINES,
            false,
            UNCONSTRAINED_WIDTH,
        );
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
            model_suffix: None,
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
    const PLAIN_CHILD: &str = "read";
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
            model_suffix: None,
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

    /// A child answering with text no renderer claimed, which is the arm that
    /// has to break its own lines rather than leave them to ratatui.
    fn plain_entry(text: &str) -> BatchToolEntry {
        BatchToolEntry {
            output: Some(ToolOutput::Plain(caudra_agent::TextOutput {
                text: text.to_owned(),
                instructions: None,
                state: None,
                lua_provenance: None,
            })),
            ..batch_entry(PLAIN_CHILD, 0)
        }
    }

    /// The child's body rows, which follow its one summary row. The card is
    /// given the child's width plus the indent it prefixes, so `width` is what
    /// the body itself ends up with.
    fn markdown_child_body(text: &str, width: u16) -> Vec<String> {
        let limits = limits(BatchViews::new([0])).with_width(width + BATCH_CHILD_INDENT_WIDTH);
        let card = render_batch(&[markdown_entry(text)], false, &limits);
        card.lines
            .iter()
            .skip(1)
            .map(|line| spans_text(&line.spans))
            .collect()
    }

    /// One paragraph longer than the card is wide. The renderer leaves
    /// paragraphs for ratatui to break at paint time, which is fine for a body
    /// that owns its column and wrong for one hanging off a tree: every
    /// continuation restarts at column zero and the trunk stops with it.
    const LONG_PARAGRAPH: &str = "A librarian walks into a library and whispers that they are \
        looking for a book on paranoia, and the librarian whispers back that the books are right \
        behind them, which is the sort of answer that only raises further questions.";

    #[test]
    fn a_markdown_child_breaks_a_paragraph_to_its_own_width() {
        let body = markdown_child_body(LONG_PARAGRAPH, NARROW_BODY_WIDTH);

        assert!(
            body.len() > 1,
            "an unbroken paragraph is one the terminal breaks for us, losing the trunk: {body:?}"
        );
        let indent = format!("{TREE_GAP}{BATCH_BODY_PAD}");
        let limit = usize::from(NARROW_BODY_WIDTH) + indent.len();
        for line in &body {
            assert!(
                line.starts_with(&indent) && line.chars().count() <= limit,
                "a row past the width is one the terminal wraps for us: {line:?}"
            );
        }
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
        let indent = format!("{TREE_GAP}{BATCH_BODY_PAD}");
        let limit = usize::from(NARROW_BODY_WIDTH) + indent.len();
        for line in &body {
            assert!(
                line.starts_with(&indent),
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

    const CARD_RECORDS_SOURCE: &str =
        "a batch card that names no source hands every child back to the scraping fallback";
    const ROWS_PER_CARD_LINE: &str =
        "a batch card needs one source row per painted line or extraction gives up on it";
    const ONE_SOURCE_LINE: &str = "the rows a paragraph broke into have to name the line it was written as, or a \
         selection across them copies the break";
    const CHILD_INDENT_MSG: &str =
        "a row that does not open on the trunk reads as belonging to the batch, not the child";

    /// A lone child's body rows, each paired with the source it names, which is
    /// what a selection over that row resolves against. The summary row above
    /// them is dropped along with any row it broke into, which all name the
    /// same heading it does.
    fn lone_child_body(card: &BatchCard) -> Vec<(String, LineProvenance)> {
        let source = card.source.clone().expect(CARD_RECORDS_SOURCE);
        assert_eq!(card.lines.len(), source.rows.len(), "{ROWS_PER_CARD_LINE}");
        let heading = source.rows.first().and_then(|row| row.line.clone());
        card.lines
            .iter()
            .map(|line| spans_text(&line.spans))
            .zip(source.rows)
            .skip_while(|(_, row)| row.line == heading)
            .collect()
    }

    fn all_indented(body: &[(String, LineProvenance)]) -> bool {
        let indent = format!("{TREE_GAP}{BATCH_BODY_PAD}");
        body.iter().all(|(text, _)| text.starts_with(&indent))
    }

    #[test]
    fn a_batch_card_names_a_source_for_every_line_it_draws() {
        let entries = [batch_entry("read", 2), batch_entry("grep", 3)];
        let card = render_batch(&entries, false, &limits(BatchViews::new([0, 1])));

        let source = card.source.as_ref().expect(CARD_RECORDS_SOURCE);
        assert_eq!(source.rows.len(), card.lines.len(), "{ROWS_PER_CARD_LINE}");
        assert!(source.names_source(), "{CARD_RECORDS_SOURCE}");
    }

    const TITLE_HANGS: &str = "a title wider than the card hangs under its own label: the \
        connector and the sigil are said once, and the trunk runs down every row it took";

    /// The reported bug: a `task` child's title with its arguments and its
    /// annotation ran past the card, and the terminal broke it at column zero,
    /// so the tree ended at the first child long enough to overflow.
    #[test]
    fn a_child_title_too_wide_for_the_card_hangs_under_its_label() {
        let long = BatchToolEntry {
            summary: "Located permission prompt command scope options repeated tool calls \
                      shell classifier reusable patterns"
                .to_owned(),
            ..batch_entry(SHELL_CHILD, 0)
        };
        let limits =
            limits(BatchViews::default()).with_width(NARROW_BODY_WIDTH + BATCH_CHILD_INDENT_WIDTH);

        let card = render_batch(&[long, batch_entry(SHELL_CHILD, 0)], false, &limits);

        let drawn: Vec<String> = card
            .lines
            .iter()
            .map(|line| spans_text(&line.spans))
            .collect();
        let title = &drawn[..drawn.len() - 1];
        assert!(title.len() > 1, "{TITLE_HANGS}: {drawn:?}");
        assert!(
            title[0].starts_with(TREE_BRANCH),
            "{TITLE_HANGS}: {title:?}"
        );
        let hang = format!("{TREE_TRUNK}{BATCH_BODY_PAD}");
        assert!(
            title.iter().skip(1).all(|row| row.starts_with(&hang)),
            "{TITLE_HANGS}: {title:?}"
        );
        assert_eq!(
            card.rows[..title.len()],
            vec![Some(RowTarget(0)); title.len()],
            "{TITLE_HANGS}"
        );
        assert_eq!(
            card.source.as_ref().map(|source| source.rows.len()),
            Some(card.lines.len()),
            "{ROWS_PER_CARD_LINE}"
        );
    }

    /// The two halves of the fix meeting: the card breaks the paragraph so each
    /// drawn row keeps the trunk, and every one of those rows still points at
    /// the whole line, so copying across the break gives back what was written.
    #[test]
    fn the_rows_a_broken_paragraph_drew_name_one_source_line() {
        let limits =
            limits(BatchViews::new([0])).with_width(NARROW_BODY_WIDTH + BATCH_CHILD_INDENT_WIDTH);
        let card = render_batch(&[markdown_entry(LONG_PARAGRAPH)], false, &limits);

        let body = lone_child_body(&card);
        let drawn: Vec<Option<Range<u32>>> = body
            .iter()
            .filter(|(_, row)| row.line.is_some())
            .map(|(_, row)| row.line.clone())
            .collect();

        assert!(all_indented(&body), "{CHILD_INDENT_MSG}: {body:?}");
        assert!(drawn.len() > 1, "{ONE_SOURCE_LINE}: {body:?}");
        assert!(
            drawn.iter().all(|line| *line == drawn[0]),
            "{ONE_SOURCE_LINE}: {body:?}"
        );
    }

    /// Text no renderer claimed breaks the same way, for the same reason: a row
    /// the terminal wraps for us restarts at column zero and drops the trunk.
    #[test]
    fn a_plain_child_breaks_an_over_wide_body_to_its_own_width() {
        let limits =
            limits(BatchViews::new([0])).with_width(NARROW_BODY_WIDTH + BATCH_CHILD_INDENT_WIDTH);
        let card = render_batch(&[plain_entry(LONG_PARAGRAPH)], false, &limits);

        let body = lone_child_body(&card);
        let written = body[0].1.line.clone().expect(ONE_SOURCE_LINE);
        let limit = usize::from(NARROW_BODY_WIDTH) + TREE_GAP.len() + BATCH_BODY_PAD.len();

        assert!(body.len() > 1, "{ONE_SOURCE_LINE}: {body:?}");
        assert!(all_indented(&body), "{CHILD_INDENT_MSG}: {body:?}");
        assert_eq!(
            (written.end - written.start) as usize,
            LONG_PARAGRAPH.len(),
            "{ONE_SOURCE_LINE}: {body:?}"
        );
        for (text, row) in &body {
            assert!(
                text.chars().count() <= limit,
                "{CHILD_INDENT_MSG}: {text:?}"
            );
            assert_eq!(row.line.as_ref(), Some(&written), "{ONE_SOURCE_LINE}");
        }
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
        let card = render_batch(&entries, false, &limits(views));
        (card.lines, card.rows)
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

    /// A lone child is the last node of its card, so its row opens on the
    /// closing connector.
    const ONLY_CHILD_CONNECTOR: &str = TREE_LAST;

    /// The row without the tree column, which every child carries and no
    /// assertion here is about.
    fn child_row(entry: BatchToolEntry) -> String {
        let row =
            line_text(&render_batch(&[entry], false, &limits(BatchViews::default())).lines[0]);
        row.strip_prefix(ONLY_CHILD_CONNECTOR)
            .expect("a child row opens on its connector")
            .to_owned()
    }

    /// The sigil sits behind the connector, which is neutral chrome.
    fn child_sigil(entry: BatchToolEntry) -> Span<'static> {
        render_batch(&[entry], false, &limits(BatchViews::default())).lines[0].spans[1].clone()
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

    /// Behind the tree connector the row leads with its sigil, which is what
    /// carries the outcome; a standalone compact row has never done otherwise.
    /// Pending and running are still told apart from a finished call by the
    /// tense of the label beside it.
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

    const TREE_MSG: &str = "a card is a tree: the last child closes it and every earlier one \
        carries the trunk down its own rows";

    /// Every child is a node, so the connector on each says whether a sibling
    /// follows it.
    #[test]
    fn a_card_closes_its_tree_on_the_last_child() {
        let entries = [
            batch_entry(SHELL_CHILD, 0),
            batch_entry(SHELL_CHILD, 1),
            batch_entry(SHELL_CHILD, 2),
        ];

        let card = render_batch(&entries, false, &limits(BatchViews::default()));

        let connectors: Vec<String> = card
            .lines
            .iter()
            .map(|line| line.spans[0].content.to_string())
            .collect();
        assert_eq!(
            connectors,
            [TREE_BRANCH, TREE_BRANCH, TREE_LAST],
            "{TREE_MSG}"
        );
    }

    const SETTLED_MSG: &str = "a settled child owns its row and its body and nothing else: a \
        tally hung off it as a node leaves the body drawn to the left of that node";

    /// The reported bug: a finished `task` child drew `└── 2.9s` as a node of
    /// the tree and then its answer beneath at a shallower indent, so reading
    /// down one child stepped back out a level.
    #[test]
    fn a_settled_child_carries_its_tally_on_its_own_row() {
        let entries = [batch_entry(SHELL_CHILD, 1), batch_entry(SHELL_CHILD, 1)];
        let mut limits = limits(BatchViews::new([0]));
        let mut progress = batching_report(Vec::new());
        progress.settle();
        limits.progress = Arc::new(HashMap::from([(0, progress)]));

        let card = render_batch(&entries, false, &limits);

        let owned: Vec<String> = card
            .lines
            .iter()
            .zip(card.rows.iter())
            .filter(|(_, row)| **row == Some(RowTarget(0)))
            .map(|(line, _)| line_text(line))
            .collect();
        let tally = SubagentProgress::tally(BATCHING_TOOLS, Duration::ZERO);
        assert!(owned[0].ends_with(&tally), "{SETTLED_MSG}: {owned:?}");
        let body_indent = format!("{TREE_TRUNK}{BATCH_BODY_PAD}");
        assert!(
            owned[1..].iter().all(|row| row.starts_with(&body_indent)),
            "{SETTLED_MSG}: {owned:?}"
        );
    }

    const BATCHING_TOOLS: u32 = 2;
    const BATCHING_SUMMARY: &str = "2 tools";

    fn batching_report(children: Vec<ActivityChild>) -> ToolProgress {
        ToolProgress::live(SubagentProgress {
            activity: SubagentActivity::batch(
                Arc::from(BATCH_TOOL_NAME),
                BATCHING_SUMMARY,
                children,
            ),
            tools: BATCHING_TOOLS,
            elapsed: Duration::ZERO,
        })
    }

    fn activity_child(tool: &str, summary: &str, status: BatchToolStatus) -> ActivityChild {
        ActivityChild {
            tool: Arc::from(tool),
            summary: summary.to_owned(),
            status,
        }
    }

    /// The reported bug: a dispatched subagent running its own batch said only
    /// `Batching 2 tools`, and what it was batching was invisible. It is a
    /// third level of the same tree, carrying the trunk of the child it
    /// belongs to.
    #[test]
    fn a_batching_child_draws_its_roster_as_a_third_level() {
        let entries = [
            batch_entry(TASK_TOOL_NAME, 0),
            batch_entry(TASK_TOOL_NAME, 1),
        ];
        let roster = vec![
            activity_child(SHELL_CHILD, "cargo check", BatchToolStatus::Running),
            activity_child(FILE_GREP_TOOL_NAME, "fn main", BatchToolStatus::Success),
        ];
        let mut limits = limits(BatchViews::default());
        limits.progress = Arc::new(HashMap::from([(0, batching_report(roster))]));

        let card = render_batch(&entries, false, &limits);

        let drawn: Vec<String> = card.lines.iter().take(4).map(line_text).collect();
        assert_eq!(
            drawn,
            [
                format!("{TREE_BRANCH}\u{2756} Delegated {TASK_TOOL_NAME} summary"),
                format!(
                    "{TREE_TRUNK}{TREE_LAST}\u{21f6} Batching {BATCHING_SUMMARY}{CHILD_ACTIVITY_SEPARATOR}{}",
                    SubagentProgress::tally(BATCHING_TOOLS, Duration::ZERO)
                ),
                format!("{TREE_TRUNK}{TREE_GAP}{TREE_BRANCH}$ Running cargo check"),
                format!("{TREE_TRUNK}{TREE_GAP}{TREE_LAST}⌕ Grepped fn main"),
            ],
            "{TREE_MSG}"
        );
        assert!(
            card.rows[..4].iter().all(|row| *row == Some(RowTarget(0))),
            "every row of a child answers for the child that owns it"
        );
    }

    fn shell_child(status: BatchToolStatus) -> BatchToolEntry {
        BatchToolEntry {
            status,
            ..batch_entry(SHELL_CHILD, 0)
        }
    }

    /// Back-dated far enough that the tenths are stable however slow the host
    /// is, and a magnitude the settled formatter spells differently.
    const CHILD_RAN_FOR: Duration = Duration::from_millis(1_201);
    const CHILD_LIVE_CLOCK: &str = " · 1.2s";
    const CHILD_MEASURED_MS: u64 = 10;
    const CHILD_MEASURED_CLOCK: &str = " · 10ms";
    const CHILD_TIMEOUT_MS: u64 = 120_000;
    const CHILD_CLOCK_MSG: &str = "a child row keeps the clock its standalone card would";

    fn started(index: usize, ago: Duration) -> RenderLimits {
        RenderLimits {
            started: Arc::new(HashMap::from([(index, Instant::now() - ago)])),
            ..limits(BatchViews::default())
        }
    }

    fn shell_child_output(duration_ms: u64) -> ToolOutput {
        ToolOutput::Shell(ShellOutput {
            model_text: String::new(),
            relative_workdir: ".".into(),
            timeout_ms: CHILD_TIMEOUT_MS,
            duration_ms,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 0,
            stdout_utf8_bytes: 0,
            stderr_utf8_bytes: 0,
            stdout: String::new(),
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 0,
            stderr_redraws_collapsed: 0,
            filter: None,
        })
    }

    #[test]
    fn a_running_shell_child_counts_up() {
        let card = render_batch(
            &[shell_child(BatchToolStatus::Running)],
            false,
            &started(0, CHILD_RAN_FOR),
        );
        let row = line_text(&card.lines[0]);

        assert!(row.ends_with(CHILD_LIVE_CLOCK), "{CHILD_CLOCK_MSG}: {row}");
    }

    /// The subprocess's own time supersedes the wall clock the row was drawn
    /// with, exactly as a standalone card's header does.
    #[test]
    fn a_settled_shell_child_reports_the_commands_own_time() {
        let entry = BatchToolEntry {
            output: Some(shell_child_output(CHILD_MEASURED_MS)),
            ..shell_child(BatchToolStatus::Success)
        };
        let card = render_batch(&[entry], false, &started(0, CHILD_RAN_FOR));
        let row = line_text(&card.lines[0]);

        assert!(
            row.ends_with(CHILD_MEASURED_CLOCK),
            "{CHILD_CLOCK_MSG}: {row}"
        );
        assert!(!row.contains(CHILD_LIVE_CLOCK), "{CHILD_CLOCK_MSG}: {row}");
    }

    /// Same row, same start time: only a child that measures itself gets one.
    #[test]
    fn a_non_shell_child_draws_no_clock() {
        let entry = BatchToolEntry {
            status: BatchToolStatus::Running,
            ..batch_entry(GREP_CHILD, 0)
        };
        let card = render_batch(&[entry], false, &started(0, CHILD_RAN_FOR));

        assert!(!line_text(&card.lines[0]).contains(CHILD_ACTIVITY_SEPARATOR));
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

    /// A separator draws nothing but the trunk it has to keep going, so it
    /// reads as air between two children while the tree stays joined.
    fn separator_rows(lines: &[Line<'static>]) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                let text = line_text(line);
                text.is_empty() || text.trim_end() == TREE_TRUNK.trim_end()
            })
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

        assert!(separator_rows(&lines).is_empty(), "{BATCH_TIGHT_MSG}");
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
        let card = render_batch(&entries, false, &limits(BatchViews::new([1])));

        assert_eq!(
            separator_rows(&card.lines),
            vec![1, 5],
            "{BATCH_BODY_AIR_MSG}"
        );
        assert_eq!(card.lines.len(), card.rows.len(), "the rows stay parallel");
    }

    /// The card's own body is the list of what it ran, so opening it says
    /// nothing about the children. Otherwise one click on a card of ten greps
    /// would write ten whole greps into the transcript.
    #[test]
    fn an_opened_card_leaves_its_children_folded() {
        let whole = 6;
        let opened = RenderLimits::new(true, PARENT_BUDGET, BatchViews::default(), TOOL_LINES);
        let entries = [batch_entry("read", whole), batch_entry("grep", whole)];
        let card = render_batch(&entries, false, &opened);
        assert_eq!(body_count(&card.lines), 0);
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
        let card = render_batch(&[write_entry(body_lines)], false, &limits(views));
        (card.lines, card.rows)
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

        let card = render_batch(&[entry], false, &limits(BatchViews::default()));

        assert_eq!(body_count(&card.lines), 0, "{CHANGED_MSG}");
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

    const GRAPH_HANGS: &str = "a graph row wider than the card hangs under its own name, so the \
        fields it ends on stay inside the card instead of restarting at column zero";
    const GRAPH_CARD_WIDTH: u16 = 44;

    /// The reported bug: a row ending in `[test]` ran past the card and the
    /// terminal broke it, dropping the tail to the left margin.
    #[test]
    fn a_graph_row_too_wide_for_the_card_hangs_under_its_name() {
        let row = CodeGraphRow {
            test_scope: true,
            ..ranked_row(
                "shell_pattern_option_is_absent_without_a_reusable_prefix",
                4,
            )
        };

        let (lines, _) = render_code_graph(
            "located in .",
            &[row],
            None,
            "[21 files]",
            20,
            false,
            GRAPH_CARD_WIDTH,
        );

        let drawn: Vec<String> = lines.iter().map(|line| spans_text(&line.spans)).collect();
        assert!(drawn.len() > 3, "{GRAPH_HANGS}: {drawn:?}");
        assert!(
            drawn
                .iter()
                .all(|row| row.chars().count() <= usize::from(GRAPH_CARD_WIDTH)),
            "{GRAPH_HANGS}: {drawn:?}"
        );
        let body = &drawn[1..drawn.len() - 1];
        assert!(
            body.iter()
                .skip(1)
                .all(|row| row.starts_with(GRAPH_HANG) && !row.starts_with(GRAPH_ROW_SIGIL)),
            "{GRAPH_HANGS}: {body:?}"
        );
    }

    #[test]
    fn a_code_graph_card_keeps_its_headline_and_footer() {
        const HEADLINE: &str = "ranked symbols in .";
        const FOOTER: &str = "[3 files, 9 symbols, 4 edges]";

        let rows = vec![ranked_row("alpha", 3), ranked_row("beta", 1)];
        let (lines, truncated) = render_code_graph(
            HEADLINE,
            &rows,
            None,
            FOOTER,
            20,
            false,
            UNCONSTRAINED_WIDTH,
        );

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
        let (lines, truncated) =
            render_code_graph(HEADLINE, &rows, None, FOOTER, 8, false, UNCONSTRAINED_WIDTH);

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
        let plain = render_code_graph(
            "h",
            &rows,
            None,
            "[3 files]",
            20,
            false,
            UNCONSTRAINED_WIDTH,
        )
        .0;
        let caveated = render_code_graph(
            "h",
            &rows,
            None,
            "[3 files; rank NOT converged after 50 iterations]",
            20,
            false,
            UNCONSTRAINED_WIDTH,
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

        let (lines, _) = render_code_graph(
            "h",
            &[near, far],
            None,
            "[f]",
            20,
            false,
            UNCONSTRAINED_WIDTH,
        );
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
        let (lines, _) = render_code_graph(
            "h",
            &[],
            Some(&source),
            "[f]",
            20,
            false,
            UNCONSTRAINED_WIDTH,
        );

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("bundle would cost more")),
            "a whole-file substitution has to say why"
        );
        assert!(texts.iter().any(|t| t.contains("fn alpha")));
    }

    const ROW_FITS: &str = "every row a card draws has to fit the width it was given; one that \
        does not is broken by the terminal instead, which starts the next row at column zero and \
        drops the gutter the card was drawing";
    const INVARIANT_WIDTH: u16 = 28;
    /// Long enough that no gutter leaves room for it, with spaces to break on
    /// so a failure is the layout's and not an unbreakable token's.
    const LONG: &str =
        "a sentence long enough that no card of this width can draw it on one row at all";

    fn text_output(text: &str) -> TextOutput {
        TextOutput {
            text: text.to_owned(),
            instructions: None,
            state: None,
            lua_provenance: None,
        }
    }

    /// Every row of the card, as the terminal would measure it.
    fn overflowing_rows(output: &ToolOutput) -> Vec<String> {
        let mut limits = limits(BatchViews::default());
        limits.width = INVARIANT_WIDTH;
        render_tool_content(None, Some(output), false, limits)
            .lines
            .iter()
            .map(line_text)
            .filter(|text| UnicodeWidthStr::width(text.as_str()) > usize::from(INVARIANT_WIDTH))
            .collect()
    }

    fn todo_output() -> ToolOutput {
        ToolOutput::TodoList(Vec::from([TodoItem {
            content: LONG.to_owned(),
            status: TodoStatus::InProgress,
            priority: Default::default(),
        }]))
    }

    fn answers_output() -> ToolOutput {
        ToolOutput::Answers(Vec::from([Answer {
            header: LONG.to_owned(),
            labels: Vec::from([LONG.to_owned()]),
        }]))
    }

    fn grep_output() -> ToolOutput {
        ToolOutput::GrepResult {
            entries: Vec::from([GrepFileEntry {
                path: format!("src/{LONG}.rs"),
                groups: Vec::from([GrepMatchGroup {
                    lines: Vec::from([GrepLine {
                        line_nr: 12,
                        text: LONG.to_owned(),
                        is_match: true,
                    }]),
                }]),
            }]),
            capped: None,
        }
    }

    fn environment_output() -> ToolOutput {
        ToolOutput::Environment {
            headline: LONG.to_owned(),
            summary: LONG.to_owned(),
            facts: Vec::from([EnvironmentFact {
                label: "platform".to_owned(),
                value: LONG.to_owned(),
            }]),
            commands: Vec::new(),
        }
    }

    fn read_code_output() -> ToolOutput {
        ToolOutput::ReadCode {
            path: "src/main.rs".to_owned(),
            start_line: 1,
            lines: Vec::from([LONG.to_owned()]),
            total_lines: 1,
            instructions: None,
        }
    }

    fn code_graph_output() -> ToolOutput {
        ToolOutput::CodeGraph {
            headline: LONG.to_owned(),
            rows: Vec::new(),
            source: None,
            footer: LONG.to_owned(),
            state: None,
        }
    }

    /// The whole point of the invariant: one case per output a card can draw,
    /// so a renderer added later has to join the list rather than quietly
    /// reintroducing the bug this fixes.
    #[test_case(todo_output() ; "todo_list")]
    #[test_case(answers_output() ; "answers")]
    #[test_case(grep_output() ; "grep_result")]
    #[test_case(environment_output() ; "environment")]
    #[test_case(read_code_output() ; "read_code")]
    #[test_case(code_graph_output() ; "code_graph")]
    #[test_case(ToolOutput::Plain(text_output(LONG)) ; "plain")]
    #[test_case(ToolOutput::Markdown(text_output(LONG)) ; "markdown")]
    fn no_row_outgrows_the_card_it_is_drawn_in(output: ToolOutput) {
        let over = overflowing_rows(&output);
        assert!(over.is_empty(), "{ROW_FITS}: {over:#?}");
    }

    const HANGS_UNDER_MARKER: &str = "a todo too long for its card carries on under its own text, \
        not against the card's edge where the marker can no longer be told from the wrap";

    /// The reported bug: a todo list inside a batch lost both the tree and its
    /// marker the moment an item was too long to fit.
    #[test]
    fn a_todo_that_outgrows_its_card_hangs_under_its_marker() {
        let mut limits = limits(BatchViews::default());
        limits.width = INVARIANT_WIDTH;

        let content = render_tool_content(None, Some(&todo_output()), false, limits);
        let rows: Vec<String> = content.lines.iter().map(line_text).collect();

        assert!(rows.len() > 1, "{HANGS_UNDER_MARKER}: {rows:#?}");
        let marker = UnicodeWidthStr::width(TodoStatus::InProgress.marker()) + 1;
        for row in rows.iter().skip(1) {
            assert!(
                row.starts_with(&" ".repeat(marker)),
                "{HANGS_UNDER_MARKER}: {row:?}"
            );
        }
    }

    const SOURCE_IS_PARALLEL: &str = "a card's source has to name one row per painted row at every \
        width, or extraction gives up on the card and copy scrapes the gutter off the screen";
    const COPY_IS_WIDTH_BLIND: &str = "what a card copies is what was written, so it cannot depend \
        on how wide the terminal happened to be";

    /// Breaking a row is a picture, not a fact about the text behind it.
    #[test_case(INVARIANT_WIDTH ; "narrow")]
    #[test_case(60 ; "medium")]
    #[test_case(UNCONSTRAINED_WIDTH ; "unconstrained")]
    fn a_card_copies_the_same_text_however_it_is_broken(width: u16) {
        let code = read_code_output();
        let mut limits = limits(BatchViews::default());
        limits.width = width;

        let content = render_tool_content(None, Some(&code), false, limits);
        let source = content.source.expect(SOURCE_IS_PARALLEL);

        assert_eq!(
            source.rows.len(),
            content.lines.len(),
            "{SOURCE_IS_PARALLEL}"
        );
        assert!(source.text.contains(LONG), "{COPY_IS_WIDTH_BLIND}");
    }
}
