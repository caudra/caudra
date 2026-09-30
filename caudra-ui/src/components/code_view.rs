use std::collections::HashMap;
use std::iter;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::animation::live_elapsed;
use crate::highlight::{fallback_span, highlight_line};
use crate::markdown::{
    LinkMap, expand_notice, should_truncate, text_to_wrapped, truncation_notice,
};
use crate::provenance::LineProvenance;
use crate::selection::wrap_breaks;
use crate::theme;

use super::tool_display::{
    ScrollTail, TREE_BRANCH, TREE_GAP, TREE_LAST, TREE_TRUNK, annotation_spans, append_annotation,
    batch_sigil_style, compact_args_for, header_spans, header_timeout, header_workdir,
    inflected_header, names_tool, progress_lines, report_header, report_markdown, report_message,
    scroll_footer_text, title,
};
use super::{
    ToolProgress, environment_card, is_collapsible, memory_card, task_card, workflow_card,
};
use caudra_agent::tools::{
    PYTHON_EXECUTION_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME, timeout_annotation,
};
use caudra_agent::types::Answer;
use caudra_agent::types::{TodoItem, TodoStatus};
use caudra_agent::{
    BatchToolEntry, BatchToolStatus, CodeGraphRow, CodeGraphSource, GrepFileEntry, INDEX_TRUNCATED,
    IndexDirectoryEntryKind, IndexLine, IndexLineSemantic, IndexOutput, IndexSourceRange,
    InstructionBlock, PatchedFile, SearchCap, SkillOutput, SubagentProgress, ToolInput, ToolOutput,
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const MAX_INSTRUCTION_LINES: usize = 15;
/// What a child's body clears past the trunk it hangs from, so its text lands
/// under the label the connector and sigil pushed across.
const BATCH_BODY_PAD: &str = "  ";
const ANSWER_MARK: &str = "  \u{2713} ";
/// An option the user passed over. Borrowed from the todo list's pending
/// marker, which is already what an unfinished circle means in a card.
const DECLINED_MARK: &str = "  \u{25cb} ";
const ANSWER_INDENT: &str = "    ";
/// Clears the option label its text explains, so a description reads as
/// belonging to the row above it rather than as another choice.
const DESCRIPTION_INDENT: &str = "      ";
const NO_ANSWER: &str = "(no answer)";
/// Indented past the child's own sigil, so the activity reads as belonging to
/// the row above it rather than as another entry in the roster.
const CHILD_ACTIVITY_SEPARATOR: &str = " · ";
/// Says a child is folded, so a row with nothing under it is not mistaken for
/// one whose body was hidden.
const BATCH_FOLDED_MARK: &str = " \u{2026}";
const QUEUED_ANNOTATION: &str = "queued";
/// What the markdown renderer calls a width it should not wrap to.
pub(super) const UNCONSTRAINED_WIDTH: u16 = 0;
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
/// The digits a numbered body reserves before it knows how far it reaches. A
/// gutter sized to the highest number drawn so far widens at ten lines and
/// again at a hundred, and each widening narrows the code column and rewraps
/// every row already on screen. Three digits covers the bodies a card usually
/// draws, costs four columns of even a forty-column card, and moves the
/// gutter once at a thousand lines instead of twice on the way there.
const MIN_GUTTER_DIGITS: usize = 3;
/// The characters a row's gutter is built from: the card's own indent, the four
/// tree glyphs, and the check a settled row opens with.
const GUTTER_CHARS: &str = " \u{2502}\u{251c}\u{2514}\u{2500}\u{2713}";
const TRUNK_GLYPH: char = '\u{2502}';
const BRANCH_GLYPH: char = '\u{251c}';
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

/// The columns `indent_all` puts back in front of every row of a child's
/// body, which is what that body's width is narrowed by before it is built.
///
/// Measured rather than counted in bytes. The connector a body hangs from is
/// `TREE_GAP` or `TREE_TRUNK` depending on whether a sibling follows, and the
/// two agree on four columns while disagreeing on four bytes against six, so a
/// byte count is right only for whichever of the pair happens to be ASCII.
fn batch_child_indent_width() -> u16 {
    (UnicodeWidthStr::width(TREE_TRUNK) + UnicodeWidthStr::width(BATCH_BODY_PAD)) as u16
}

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

/// The columns a code body holds back for its line numbers, given the highest
/// number its whole line range reaches rather than the highest it happens to
/// be drawing. Reserved so that growing content, a raised budget or a moved
/// window cannot re-gutter rows that are already on screen.
fn gutter_digits(max_nr: usize) -> usize {
    nr_width(max_nr).max(MIN_GUTTER_DIGITS)
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
    pub source: Range<u32>,
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

    /// The rows a window or a budget kept, giving the body up when they can no
    /// longer be told to line up. The blocks move with them, or a fence would
    /// land around rows the window never drew.
    pub(crate) fn keep_rows(mut self, kept: Range<usize>) -> Option<Self> {
        self.rows = self.rows.get(kept.clone())?.to_vec();
        self.code = self
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
        Some(self)
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
        let extension = Path::new(path)
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
            code.source.start += base;
            code.source.end += base;
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

fn code_row(spans: &mut Vec<Span<'static>>, text: &str, at: u32) -> LineProvenance {
    let line = at..at + text.len() as u32;
    let mut sources = vec![SpanSource::Chrome];
    if !text.contains('\t') {
        let mut offset = at;
        for span in spans.iter().skip(1) {
            let end = offset + span.content.len() as u32;
            sources.push(SpanSource::Range(Source::verbatim(offset..end)));
            offset = end;
        }
        return LineProvenance {
            line: Some(line),
            spans: sources,
        };
    }
    let mut mapped = vec![spans[0].clone()];
    let mut offset = 0;
    let mut tab_remaining = 0;
    for span in spans.iter().skip(1) {
        let mut drawn = 0;
        while drawn < span.content.len() {
            let start = at + offset as u32;
            if tab_remaining != 0 || text.as_bytes().get(offset) == Some(&b'\t') {
                if tab_remaining == 0 {
                    tab_remaining = caudra_highlight::TAB_SPACES.len();
                }
                let len = tab_remaining.min(span.content.len() - drawn);
                mapped.push(Span::styled(
                    span.content[drawn..drawn + len].to_owned(),
                    span.style,
                ));
                sources.push(SpanSource::Range(Source::atomic(start..start + 1)));
                drawn += len;
                tab_remaining -= len;
                if tab_remaining == 0 {
                    offset += 1;
                }
            } else {
                let remaining = &text[offset..];
                let len = remaining
                    .find('\t')
                    .unwrap_or(remaining.len())
                    .min(span.content.len() - drawn);
                if len == 0 {
                    break;
                }
                mapped.push(Span::styled(
                    span.content[drawn..drawn + len].to_owned(),
                    span.style,
                ));
                sources.push(SpanSource::Range(Source::verbatim(
                    start..start + len as u32,
                )));
                offset += len;
                drawn += len;
            }
        }
    }
    *spans = mapped;
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
    // The body's whole range, not the part of it being drawn: a budget that
    // opens or a window that moves must not change the column the code starts
    // in, or every row on screen rewraps around it.
    let extent = start_line + total_count.max(display_count).saturating_sub(1);
    let w = gutter_digits(extent);

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
        let row = code_row(&mut spans, text, at);
        at += text.len() as u32 + 1;
        let broken = wrap_styled(spans, 1, &hang, width);
        source.rows.extend(wrapped_provenance(&broken, &row));
        lines.extend(wrapped_lines(broken));
    }
    source.code.push(CodeBlock {
        rows: 0..lines.len(),
        source: 0..source.text.len() as u32,
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
/// Each hunk restarts the highlighter at its own first line, because there is
/// no file above it to parse. That is the same approximation an edit's before
/// and after fragments already get, and it costs a hunk opening inside a block
/// comment or a long string the colour it would have had in place.
fn render_unified_patch(
    patch: &str,
    syntax: Option<&'static SyntaxReference>,
    width: u16,
) -> Vec<Line<'static>> {
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
        // The wire order and the re-diffed order walk the same body, drop the
        // same no-newline markers, and so consume these two sides line for
        // line either way.
        let (before, after) = hunk.sides();
        let mut walkers = syntax.map(|s| (FileWalker::new(&before, s), FileWalker::new(&after, s)));
        let groups = if hunk.body.len() > MAX_REDIFF_LINES {
            vec![DiffHunk {
                before_start: 1,
                after_start: 1,
                lines: hunk.wire_lines(),
            }]
        } else {
            compute_hunks(&before, &after)
        };
        for group in groups {
            if !lines.is_empty() {
                lines.push(gap_ellipsis());
            }
            // A group starts where the hunk's own sides say it does, which is
            // not where the gutter starts counting.
            if let Some((before, after)) = walkers.as_mut() {
                before.skip_to(group.before_start);
                after.skip_to(group.after_start);
            }
            let mut cursor = (
                hunk.before_start + group.before_start - 1,
                hunk.after_start + group.after_start - 1,
            );
            for dl in &group.lines {
                lines.push(render_hunk_line(
                    dl,
                    walkers.as_mut(),
                    &mut cursor,
                    gutter,
                    width,
                ));
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

/// The form as the user answered it: the question, then every option it
/// offered, marked with whether it was taken. A card that showed the picks
/// alone left the reader holding an answer with nothing to read it against,
/// and the choices declined are half of what a decision was.
///
/// The options are filled back in from the tool call's input when the session
/// loads, so an answer that arrives without them is one whose input no longer
/// lines up. That still has its picks, and they are drawn the way they always
/// were rather than dropped.
fn render_answers(answers: &[Answer], budget: usize) -> (Vec<Line<'static>>, bool) {
    let t = theme::current();
    let mut lines = Vec::new();
    for (index, answer) in answers.iter().enumerate() {
        if lines.len() >= budget {
            return (lines, true);
        }
        if index > 0 {
            lines.push(Line::default());
        }
        let label = if answer.header.is_empty() {
            format!("Q{}", index + 1)
        } else {
            answer.header.clone()
        };
        lines.push(Line::styled(label, t.tool_prefix));
        if !answer.question.is_empty() {
            lines.push(Line::from(Vec::from([
                Span::styled(ANSWER_INDENT, t.tool_dim),
                Span::styled(answer.question.clone(), t.tool_dim),
            ])));
        }
        for option in &answer.options {
            let picked = answer.labels.contains(&option.label);
            lines.extend(option_lines(&option.label, picked, &t));
            if !option.description.is_empty() {
                lines.push(Line::from(Vec::from([
                    Span::styled(DESCRIPTION_INDENT, t.tool_dim),
                    Span::styled(option.description.clone(), t.tool_dim),
                ])));
            }
        }
        // What the user typed rather than picked, and every pick at all when
        // the options could not be recovered.
        for typed in answer
            .labels
            .iter()
            .filter(|label| !answer.options.iter().any(|o| o.label == **label))
        {
            lines.extend(option_lines(typed, true, &t));
        }
        if answer.labels.is_empty() {
            lines.push(Line::from(Vec::from([
                Span::styled(ANSWER_INDENT, t.tool_dim),
                Span::styled(NO_ANSWER, t.tool_dim),
            ])));
        }
    }
    let truncated = lines.len() > budget;
    lines.truncate(budget);
    (lines, truncated)
}

/// One option, marked taken or passed over. A label may hold the newlines of an
/// answer the user typed, and each of its rows hangs under the first rather
/// than restarting against the mark.
fn option_lines(label: &str, picked: bool, t: &theme::Theme) -> Vec<Line<'static>> {
    let (mark, style) = match picked {
        true => (ANSWER_MARK, t.todo_completed),
        false => (DECLINED_MARK, t.todo_pending),
    };
    label
        .lines()
        .enumerate()
        .map(|(row, piece)| {
            let prefix = if row == 0 { mark } else { ANSWER_INDENT };
            Line::from(Vec::from([
                Span::styled(prefix, style),
                Span::styled(piece.to_owned(), style),
            ]))
        })
        .collect()
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
    let mut highlights = Vec::new();
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut links = LinkMap::default();
    let mut spans_out = Vec::new();
    let mut trace = SourceTrace::default();
    let mut previous_has_body = false;
    for (index, entry) in entries.iter().enumerate() {
        let view = limits.child(index, entry);
        let progress = limits.progress.get(&index).filter(|p| p.is_live());
        let progress_window = progress.and_then(|_| {
            limits
                .child_scroll
                .get(&index)
                .copied()
                .or_else(|| limits.policy.window(&entry.tool, 0, true))
        });
        // Resolved before the summary row so the separator below knows whether
        // this child is a list entry or a block.
        let body = view.map(|mut child| {
            if progress_window.is_some() {
                child.scroll = None;
                child.budget = usize::MAX;
            }
            child_body(entry, highlight, &child, limits.live.get(&index))
        });
        let has_body = body.as_ref().is_some_and(|body| !body.lines.is_empty());
        // Every row of a child answers for it, whether or not a click would
        // change what is drawn: this is also how a dispatched child's rows are
        // traced back to the subagent they belong to.
        let target = Some(RowTarget::Item(index));
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
        let (sigil, label, tense) = title(&entry.tool, entry.status.into(), entry.status.stage());
        let report_title = report_header(&entry.tool, &entry.summary, entry.raw_input.as_ref());
        let inflected = inflected_header(&entry.tool, &report_title, tense);
        // Once the body carries the script, the row keeps only what the body
        // does not say. The same trade a card's header makes, and for the same
        // reason: the body's copy is the numbered, highlighted one.
        let summary = match has_body
            && (body_repeats_summary(entry) || matches!(entry.output, Some(ToolOutput::Tasks(_))))
        {
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
        let workdir = header_workdir(
            &entry.tool,
            entry.raw_input.as_ref(),
            entry.output.as_ref(),
            limits.cwd.as_deref(),
        );
        spans.extend(annotation_spans(
            child_annotation(entry, limits.progress.get(&index)).as_deref(),
            workdir,
        ));
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
        let mut body = body.map(|body| body.indented(continuation));
        if let Some(progress) = progress {
            let mut combined =
                body.unwrap_or_else(|| ChildBody::traced(Vec::new(), BodySource::default()));
            let history = progress_lines(progress, continuation, limits.width);
            if let Some(source) = combined.source.as_mut() {
                for line in &history {
                    source.push_chrome(line.spans.len());
                }
            }
            let history_start = combined.lines.len();
            combined.links.rows.extend(LinkMap::none_for(&history).rows);
            combined.lines.extend(history);
            combined.rows.resize(combined.lines.len(), None);
            combined = child_view(
                combined,
                progress_window,
                usize::MAX,
                ScrollTail::Live,
                &format!("{continuation}{BATCH_BODY_PAD}"),
            );
            if let Some(span) = combined.span.as_mut() {
                span.history_start = Some(history_start);
            }
            body = Some(combined);
        }
        if let Some(body) = body {
            highlights.extend(body.highlights.into_iter().map(|mut region| {
                region.path.insert(0, index);
                region.shift(lines.len());
                region
            }));
            if let Some(span) = body.span {
                spans_out.push(span.shifted(lines.len(), Some(index)));
            }
            rows.extend((0..body.lines.len()).map(|row| {
                if let Some(output) = entry.output.as_ref()
                    && let Some(Some(task)) = body.rows.get(row)
                    && let Some(task) = task_card::target_index(output, *task)
                {
                    Some(RowTarget::Task { child: index, task })
                } else {
                    target
                }
            }));
            match body.source {
                Some(source) => trace.record(lines.len(), source),
                None => trace.abandon(),
            }
            links
                .rows
                .extend(LinkMap::none_for(&lines[links.rows.len()..]).rows);
            links.rows.extend(body.links.rows);
            lines.extend(body.lines);
        }
    }
    links
        .rows
        .extend(LinkMap::none_for(&lines[links.rows.len()..]).rows);
    BatchCard {
        highlights,
        links,
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
    highlights: Vec<HighlightRegion>,
    links: LinkMap,
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
    entry.input.is_some()
        || report_message(&entry.tool, entry.raw_input.as_ref()).is_some()
        || entry
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
///
/// A dispatching child's tally leads, so a row reads the way its own card's
/// header does: what the run did, then what the call has to say about it. It
/// rides the row running as well as settled, because hung off the child as a
/// node it would be a branch of the tree holding nothing but a clock, with the
/// child's own body drawn to the left of it.
fn child_annotation(entry: &BatchToolEntry, progress: Option<&ToolProgress>) -> Option<String> {
    let own = entry.annotation.clone().or_else(|| match entry.status {
        BatchToolStatus::Pending => Some(QUEUED_ANNOTATION.to_owned()),
        BatchToolStatus::Success => entry.output.as_ref().and_then(ToolOutput::annotation),
        BatchToolStatus::Drafting
        | BatchToolStatus::AwaitingApproval
        | BatchToolStatus::Running
        | BatchToolStatus::Error => None,
    });
    let tally =
        progress.map(|progress| SubagentProgress::tally(progress.report.tools, progress.elapsed()));
    let mut annotation = match (tally, own) {
        (Some(tally), Some(own)) => Some(format!("{tally}{CHILD_ACTIVITY_SEPARATOR}{own}")),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    };
    // The same deadline a standalone card names, on the same terms: a child is
    // a whole call, and the one folded out of its brackets to be said here.
    if let Some(timeout) = header_timeout(&entry.tool, entry.raw_input.as_ref()) {
        append_annotation(&mut annotation, &timeout_annotation(timeout));
    }
    annotation
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
            .map(|started| live_elapsed(*started));
    }
    match entry.output.as_ref() {
        Some(ToolOutput::Shell(output)) => Some(Duration::from_millis(output.duration_ms)),
        _ => None,
    }
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
    if let Some(message) = report_message(&entry.tool, entry.raw_input.as_ref()) {
        let (mut lines, source, mut links) = markdown_body(&report_markdown(message), limits.width);
        let mut trace = SourceTrace::default();
        trace.record(0, source);
        if let Some(output) = output {
            let (status, source) = plain_body(&output.as_text(), limits.width);
            trace.record(lines.len(), source);
            links.rows.extend(LinkMap::none_for(&status).rows);
            lines.extend(status);
        }
        let source = trace.finish(&lines);
        return ChildBody {
            highlights: Vec::new(),
            rows: vec![None; lines.len()],
            lines,
            links,
            source,
            truncation: false,
            span: None,
        };
    }
    if entry.status != BatchToolStatus::Error
        && let Some(ToolOutput::Markdown(text)) = output
    {
        let (lines, source, links) = markdown_body(&text.text, limits.width);
        let mut body = ChildBody::traced(lines, source);
        body.links = links;
        let body = child_view(body, limits.scroll, limits.budget, ScrollTail::Settled, "");
        return with_script(entry, highlight, limits, body);
    }
    // Every answer that is text goes through one place, so no arm can be the
    // one that forgets the script. `render_tool_content` draws the script
    // itself, which is why structured output is the exception here rather than
    // a case alongside the others.
    let text = if entry.status == BatchToolStatus::Error
        && !matches!(output, Some(ToolOutput::Tasks(_)))
    {
        Some(plain_body(
            &output.map_or(String::new(), ToolOutput::as_text),
            limits.width,
        ))
    } else if let Some(tail) = live.filter(|text| output.is_none() && !text.is_empty()) {
        Some(plain_body(tail, limits.width))
    } else {
        match output {
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
            let body = child_view(
                ChildBody::traced(lines, source),
                limits.scroll,
                limits.budget,
                tail,
                "",
            );
            with_script(entry, highlight, limits, body)
        }
        None => render_tool_content(entry.input.as_ref(), output, highlight, limits.clone()).into(),
    }
}

/// A child's body with everything that has to stay parallel to its lines.
///
/// The rows travel with the lines because a window, a budget and a script each
/// change the line count between here and the card: a row one of them left
/// behind points a copy at text that was never drawn.
struct ChildBody {
    highlights: Vec<HighlightRegion>,
    links: LinkMap,
    lines: Vec<Line<'static>>,
    rows: Vec<Option<RowTarget>>,
    /// `None` where a renderer in the body named no source at all, which hands
    /// the card it lands in back to the scraping fallback.
    source: Option<BodySource>,
    truncation: bool,
    span: Option<ScrollSpan>,
}

impl ChildBody {
    fn traced(lines: Vec<Line<'static>>, source: BodySource) -> Self {
        Self {
            highlights: Vec::new(),
            links: LinkMap::none_for(&lines),
            rows: vec![None; lines.len()],
            lines,
            source: Some(source),
            truncation: false,
            span: None,
        }
    }

    fn keep_rows(&mut self, kept: Range<usize>) {
        self.highlights.retain_mut(|region| region.keep(&kept));
        self.links.rows = self
            .links
            .rows
            .get(kept.clone())
            .unwrap_or_default()
            .to_vec();
        self.rows = self.rows.get(kept.clone()).unwrap_or_default().to_vec();
        self.source = self.source.take().and_then(|source| source.keep_rows(kept));
    }

    fn indented(mut self, continuation: &str) -> Self {
        for region in &mut self.highlights {
            region.indent(
                format!("{continuation}{BATCH_BODY_PAD}"),
                theme::current().tool_dim,
            );
        }
        for row in &mut self.links.rows {
            row.insert(0, None);
        }
        self.lines = indent_all(self.lines, continuation);
        self.source = self.source.map(BodySource::indented);
        self
    }

    /// Marks a line the body was closed with, which is chrome wherever it came
    /// from: a scroll footer or a truncation notice.
    fn push_chrome(&mut self) {
        self.links.rows.push(vec![None]);
        self.rows.push(None);
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

impl From<ToolContent> for ChildBody {
    fn from(content: ToolContent) -> Self {
        Self {
            highlights: content.highlights,
            links: content.links,
            lines: content.lines,
            rows: content.rows,
            source: content.source,
            truncation: content.truncation,
            span: None,
        }
    }
}

/// `body` drawn under `top`, with everything that has to stay parallel to the
/// lines moved down with it. `gap` sets a blank row between the two when both
/// have rows to separate.
fn stacked(top: ChildBody, body: ChildBody, gap: bool) -> ChildBody {
    let mut lines = top.lines;
    if gap && !lines.is_empty() && !body.lines.is_empty() {
        lines.push(Line::default());
    }
    let shift = lines.len();
    let mut highlights = top.highlights;
    highlights.extend(body.highlights.into_iter().map(|mut region| {
        region.shift(shift);
        region
    }));
    let mut links = top.links;
    links
        .rows
        .extend(LinkMap::none_for(&lines[links.rows.len()..]).rows);
    links.rows.extend(body.links.rows);
    let mut rows = top.rows;
    rows.resize(shift, None);
    rows.extend(body.rows);
    lines.extend(body.lines);
    // The two halves were painted from different texts, so their ranges are
    // rebased onto the one the card ends up holding rather than spliced.
    let mut trace = SourceTrace::default();
    for (start, source) in [(0, top.source), (shift, body.source)] {
        match source {
            Some(source) => trace.record(start, source),
            None => trace.abandon(),
        }
    }
    ChildBody {
        highlights,
        source: trace.finish(&lines),
        links,
        lines,
        rows,
        truncation: top.truncation || body.truncation,
        span: body.span.map(|span| span.shift_lines(shift)),
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
    stacked(script.into(), body, true)
}

/// A loaded skill: the place it was loaded from, then its instructions drawn
/// as the document they are rather than as the numbered text the model reads.
///
/// The place stands outside the budget, so an abridged card still opens on the
/// start of the skill instead of spending one of its rows on where it lives.
fn skill_body(skill: &SkillOutput, limits: &RenderLimits) -> ChildBody {
    let (lines, source, links) = markdown_body(&skill.body, limits.width);
    let mut document = ChildBody::traced(lines, source);
    document.links = links;
    let (mut location, mut source) = plain_body(&skill.location, limits.width);
    // A location is not code, so a copy reaching past it must not fence it.
    source.code.clear();
    let style = theme::current().tool_dim;
    for span in location.iter_mut().flat_map(|line| line.spans.iter_mut()) {
        span.style = style;
    }
    stacked(
        ChildBody::traced(location, source),
        document.capped(limits.budget),
        false,
    )
}

/// Holds a child to its window when it scrolls and to its budget otherwise.
///
/// The window takes the footer the budget's notice would have taken. Both say
/// what is not being shown; a window says it as two edges and which one the
/// reader is pinned to, because that is what tells them whether output is
/// still arriving under what they are reading.
fn child_view(
    mut body: ChildBody,
    scroll: Option<ScrollWindow>,
    budget: usize,
    tail: ScrollTail,
    footer_indent: &str,
) -> ChildBody {
    let Some(window) = scroll else {
        return body.capped(budget);
    };
    let total = body.lines.len();
    let (lines, hidden) = window_rows(std::mem::take(&mut body.lines), Some(window));
    let (start, below) = hidden.unwrap_or_default();
    body.lines = lines;
    body.keep_rows(start..start + body.lines.len());
    // Published before the footer is decided, and against the rows the window
    // kept rather than the line the footer would add to them. A body that
    // fits has nothing to say in a footer and is still drawn in a window, and
    // a card's own body already publishes that case from `push_live_body`.
    body.span = Some(ScrollSpan {
        child: None,
        first: 0,
        lines: body.lines.len(),
        extent_lines: body.lines.len(),
        total,
        offset: start,
        history_start: None,
    });
    let Some(mut footer) = scroll_footer_text(start, below, tail) else {
        return body;
    };
    footer.insert_str(0, footer_indent);
    body.lines
        .push(Line::from(Span::styled(footer, theme::current().tool_dim)));
    body.push_chrome();
    body.truncation = true;
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
/// Breaking a paragraph here does not put a newline on the clipboard: the rows
/// of one source line all name that line, so copy reads it back as it was
/// written rather than as it was drawn.
pub(super) fn markdown_body(text: &str, width: u16) -> (Vec<Line<'static>>, BodySource, LinkMap) {
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
        painted.links,
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
        source: 0..text.len() as u32,
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
        // A gutter deep enough to leave no room to break into still may not run
        // past the card: the terminal would break it instead, at column zero,
        // taking the gutter with it and cutting the tree the gutter was drawing.
        // The pieces go on answering for every byte, so a copy reads the row
        // whole however little of it reached the screen.
        let mut left = match width != UNCONSTRAINED_WIDTH && room < MIN_WRAP_COLUMNS {
            true => room,
            false => usize::MAX,
        };
        let mut row = head;
        for (origin, span) in spans.into_iter().enumerate().skip(gutter) {
            let text = span.content.as_ref();
            let drawn = clipped(text, &mut left);
            row.push(SpanPiece {
                origin: Some(origin),
                offset: 0,
                len: text.len(),
                span: Span::styled(drawn, span.style),
            });
        }
        return Vec::from([row]);
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
            for piece in row.iter_mut().rev() {
                if piece.origin.is_none_or(|origin| origin < gutter) {
                    break;
                }
                let kept = piece.span.content.trim_end_matches(' ').len();
                if kept != piece.span.content.len() {
                    piece.span.content.to_mut().truncate(kept);
                }
                if kept != 0 {
                    break;
                }
            }
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

/// As much of `text` as `room` has left, in whole characters, drawing down what
/// it took. `usize::MAX` is a row under no pressure, which is every row wide
/// enough to have been broken instead.
fn clipped(text: &str, room: &mut usize) -> String {
    if *room == usize::MAX {
        return text.to_owned();
    }
    let mut out = String::new();
    for glyph in text.chars() {
        let width = UnicodeWidthChar::width(glyph).unwrap_or_default();
        if width > *room {
            *room = 0;
            break;
        }
        *room -= width;
        out.push(glyph);
    }
    out
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

/// The left column a wrapped row carries on under.
///
/// A break must not decide the shape of the tree. A row with a sibling below it
/// keeps its trunk, and one that closed its branch keeps the blank the connector
/// already promised, so a child reads as owning its wrapped rows exactly as it
/// owns its first. Everything else a gutter draws — a line number, a marker, a
/// sigil — is said once, on the row it was drawn for.
///
/// Each character is replaced by one of the same display width, so the text
/// under the hang stays in the column the unbroken row put it in.
fn hang_under(gutter: &str) -> String {
    gutter
        .chars()
        .map(|glyph| match glyph {
            TRUNK_GLYPH | BRANCH_GLYPH => String::from(TRUNK_GLYPH),
            other => " ".repeat(UnicodeWidthChar::width(other).unwrap_or_default()),
        })
        .collect()
}

/// One row broken to `width`, hung under whatever gutter it opens with.
fn wrap_row(spans: Vec<Span<'static>>, gutter: usize, width: u16) -> Vec<Vec<SpanPiece>> {
    let gutter = gutter
        .max(gutter_spans(&spans))
        .min(spans.len().saturating_sub(1));
    let hang: String = spans
        .iter()
        .take(gutter)
        .map(|span| hang_under(span.content.as_ref()))
        .collect();
    wrap_styled(spans, gutter, &hang, width)
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
    /// `head` is how many leading spans of row 0 are the card's own head: the
    /// indicator and the sigil, which the row that carries them declares
    /// because nothing about their text says so.
    pub(crate) fn new(lines: Vec<Line<'static>>, head: usize, width: u16) -> Self {
        Self {
            per_line: lines
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let declared = if index == 0 { head } else { 0 };
                    wrap_row(line.spans, declared, width)
                })
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

    /// [`Self::row_of`] for many lines, counting the rows once for them all.
    pub(crate) fn rows_of(&self, lines: &[usize]) -> Vec<usize> {
        let starts = self.starts();
        lines
            .iter()
            .map(|&line| starts[line.min(self.per_line.len())])
            .collect()
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

    /// A window recorded against lines, moved onto the rows those lines broke
    /// into. The extent travels with the start: a window whose lines wrapped
    /// paints taller than it was recorded, and a track left at the old count
    /// is a bar shorter than the body it sits beside.
    pub(crate) fn scroll_span(&self, span: ScrollSpan) -> ScrollSpan {
        let rows = self.range(span.first..span.first + span.lines);
        ScrollSpan {
            first: rows.start,
            lines: rows.end - rows.start,
            ..span
        }
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
fn render_patch(files: &[PatchedFile], highlight: bool, width: u16) -> Vec<Line<'static>> {
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
        lines.extend(render_unified_patch(
            &file.patch,
            highlight.then(|| caudra_highlight::syntax_for_path(&file.path)),
            width,
        ));
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
/// `MessagesPanel::flush_dirty_cards`. Rendering it per token cost the file's
/// length squared, which is what a long write used to feel like as lag.
///
/// `render_code` is told the window is the whole of what it is drawing, so it
/// reports nothing hidden: the rest of the file has not arrived yet, and
/// there is nothing a click could reveal.
///
/// Nothing here is highlighted. A file cut off mid-token leaves the parser in
/// a state the rest of the file has not justified yet.
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

/// How much of a body fits under `room`, given that the notice about the rest
/// costs a row of its own and has to come out of the same budget.
pub(super) fn body_window(total: usize, room: usize) -> (usize, usize) {
    let (shown, hidden) = within(total, room);
    if hidden == 0 {
        return (shown, hidden);
    }
    within(total, room.saturating_sub(1))
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
        "cuda" => "cpp",
        "objc" => "Objective-C",
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

/// The rows a window shows, with the counts it hides above and below. `None`
/// for a body drawn to a budget instead, which keeps every row it painted.
///
/// Cut from painted rows rather than from the source lines behind them. A
/// window is a height in terminal rows, and one source line is any number of
/// rows wide once it wraps, so windowing the source makes the card's height
/// follow the length of whatever happens to be in view.
pub(crate) fn window_rows<T>(
    mut rows: Vec<T>,
    window: Option<ScrollWindow>,
) -> (Vec<T>, Option<(usize, usize)>) {
    let Some(window) = window else {
        return (rows, None);
    };
    let total = rows.len();
    let (start, end) = window.range(total);
    rows.truncate(end);
    (rows.split_off(start), Some((start, total - end)))
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
    /// Whether a level above this one on the same nesting path already draws
    /// an output body of its own.
    ///
    /// One body per path is the whole point: a leaf that gains a row scrolls
    /// inside that one body instead of growing its parent, which grows its
    /// parent, until some level happens to hit a cap. Tree rows are untouched
    /// — every level still draws its roster, its live status and its clocks —
    /// so what is held to one is the *output* a path scrolls, never what it
    /// says it is doing.
    pub body_taken: bool,
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
    /// The session's working directory, which a child's `workdir` argument is
    /// resolved against until its result says where the child ran.
    pub cwd: Option<Arc<Path>>,
}

impl RenderLimits {
    pub fn new(full: bool, budget: usize, views: BatchViews, tool_lines: ToolOutputLines) -> Self {
        Self {
            budget: if full { usize::MAX } else { budget },
            scroll: None,
            body_taken: false,
            policy: CardPolicy::default(),
            child_scroll: Arc::default(),
            views,
            progress: ChildProgress::default(),
            live: ChildLive::default(),
            started: ChildStarted::default(),
            tool_lines,
            width: UNCONSTRAINED_WIDTH,
            cwd: None,
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

    pub fn with_cwd(self, cwd: Option<Arc<Path>>) -> Self {
        Self { cwd, ..self }
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
    /// A path already carrying a bounded body draws no second one. The level
    /// above is where that path's growth is absorbed, so a child under it
    /// keeps its row — sigil, status, annotation, counts, clock — and its own
    /// roster, and folds its output away instead of opening another window
    /// for every ancestor to grow around. Clicking the row is still the way
    /// in, and it is what moves the body down to this level: an opened child
    /// draws whole, bounds nothing, and hands the path on to whatever *it*
    /// nests.
    ///
    /// The views and the reports name this card's children, so both are
    /// dropped on the way in or a nested batch would read them as its own.
    fn child(&self, index: usize, entry: &BatchToolEntry) -> Option<Self> {
        let open = self.views.is_open(index);
        if (self.policy.compact || self.body_taken) && !open {
            return None;
        }
        // A script arrives before the call it belongs to is dispatched, so a
        // child that has one is worth watching a frame earlier than one whose
        // only claim is output it has started to print.
        let streaming = match entry.output.as_ref() {
            None => {
                entry.input.is_some() || self.live.get(&index).is_some_and(|tail| !tail.is_empty())
            }
            Some(ToolOutput::Tasks(tasks)) => tasks.iter().any(|task| task.active()),
            Some(_) => false,
        };
        if !open
            && (self.policy.stays_collapsed(&entry.tool)
                || (!streaming && is_collapsible(entry.effect, &entry.tool)))
        {
            return None;
        }
        let scroll = self
            .child_scroll
            .get(&index)
            .copied()
            .or_else(|| self.policy.window(&entry.tool, 0, true));
        let budget = match scroll {
            Some(window) => window.height,
            None if open => usize::MAX,
            None => self.tool_lines.get(&entry.tool),
        };
        Some(Self {
            budget,
            scroll,
            // A child the reader opened draws whole, which is them asking for
            // what is under it too, so it hands the path on rather than
            // claiming the one body for itself.
            body_taken: budget != usize::MAX,
            policy: self.policy.clone(),
            child_scroll: Arc::default(),
            views: BatchViews::default(),
            progress: ChildProgress::default(),
            live: ChildLive::default(),
            started: ChildStarted::default(),
            tool_lines: self.tool_lines,
            width: self.width.saturating_sub(batch_child_indent_width()),
            cwd: self.cwd.clone(),
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
pub enum RowTarget {
    Item(usize),
    Task { child: usize, task: usize },
}

impl RowTarget {
    pub fn index(self) -> usize {
        match self {
            Self::Item(index) => index,
            Self::Task { child, .. } => child,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeRole {
    Input,
    Output,
}

#[derive(Clone, PartialEq, Eq)]
pub enum HighlightTransform {
    Indent(String, Style),
    Wrap(u16),
    Keep(Range<usize>),
}

#[derive(Clone)]
pub struct HighlightRegion {
    pub range: Range<usize>,
    pub path: Vec<usize>,
    pub role: CodeRole,
    pub limits: RenderLimits,
    pub transforms: Vec<HighlightTransform>,
}

impl HighlightRegion {
    fn new(range: Range<usize>, role: CodeRole, limits: &RenderLimits) -> Self {
        let budget = match role {
            CodeRole::Input => usize::MAX,
            CodeRole::Output => limits.budget,
        };
        Self {
            range,
            path: Vec::new(),
            role,
            limits: RenderLimits {
                budget,
                width: limits.width,
                ..RenderLimits::default()
            },
            transforms: Vec::new(),
        }
    }

    pub fn shift(&mut self, by: usize) {
        self.range = self.range.start + by..self.range.end + by;
    }

    pub fn indent(&mut self, indent: String, style: Style) {
        self.transforms
            .push(HighlightTransform::Indent(indent, style));
    }

    pub(crate) fn wrap(&mut self, wrapped: &WrappedRows, width: u16) {
        self.range = wrapped.range(self.range.clone());
        self.transforms.push(HighlightTransform::Wrap(width));
    }

    pub(crate) fn keep(&mut self, kept: &Range<usize>) -> bool {
        let start = self.range.start.max(kept.start);
        let end = self.range.end.min(kept.end);
        if start >= end {
            return false;
        }
        self.transforms.push(HighlightTransform::Keep(
            start - self.range.start..end - self.range.start,
        ));
        self.range = start - kept.start..end - kept.start;
        true
    }

    pub fn render(&self, input: Option<&ToolInput>, output: Option<&ToolOutput>) -> ToolContent {
        self.render_with_highlight(input, output, true)
    }

    pub fn render_fallback(&self, input: &ToolInput) -> ToolContent {
        self.render_with_highlight(Some(input), None, false)
    }

    fn render_with_highlight(
        &self,
        input: Option<&ToolInput>,
        output: Option<&ToolOutput>,
        highlight: bool,
    ) -> ToolContent {
        let mut content = render_tool_content(input, output, highlight, self.limits.clone());
        for transform in self.transforms.iter().skip(1) {
            match transform {
                HighlightTransform::Indent(indent, style) => {
                    for line in &mut content.lines {
                        line.spans.insert(0, Span::styled(indent.clone(), *style));
                    }
                    for row in &mut content.links.rows {
                        row.insert(0, None);
                    }
                    content.source = content.source.map(BodySource::indented);
                }
                HighlightTransform::Wrap(width) => content = wrapped_content(content, *width),
                HighlightTransform::Keep(range) => {
                    content.lines = content
                        .lines
                        .get(range.clone())
                        .unwrap_or_default()
                        .to_vec();
                    content.rows = content.rows.get(range.clone()).unwrap_or_default().to_vec();
                    content.links.rows = content
                        .links
                        .rows
                        .get(range.clone())
                        .unwrap_or_default()
                        .to_vec();
                    content.source = content
                        .source
                        .and_then(|source| source.keep_rows(range.clone()));
                }
            }
        }
        content
    }
}

pub struct ToolContent {
    pub highlights: Vec<HighlightRegion>,
    pub(crate) links: LinkMap,
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
    pub extent_lines: usize,
    /// Lines of body the window is a view onto, and how far down it sits.
    pub total: usize,
    pub offset: usize,
    pub history_start: Option<usize>,
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
    let mut highlights = Vec::new();
    let mut output_highlights = Vec::new();
    let mut truncation = false;
    let mut output_rows: Vec<Option<RowTarget>> = Vec::new();
    let mut output_spans: Vec<ScrollSpan> = Vec::new();
    let mut trace = SourceTrace::default();
    let mut output_source: Option<BodySource> = None;
    let mut output_links = LinkMap::default();
    let drawn_script = input.map(|i| match i {
        ToolInput::Script { language, code } | ToolInput::Code { language, code } => {
            (language, code)
        }
    });
    if let Some((language, code)) = drawn_script {
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
        if !highlight && !lines.is_empty() {
            highlights.push(HighlightRegion::new(
                0..lines.len(),
                CodeRole::Input,
                &limits,
            ));
        }
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
            render_patch(files, highlight, limits.width),
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
        Some(ToolOutput::Memory(output)) => {
            let (card_lines, rows, truncated) =
                memory_card::render(output, limits.bounded_budget(), limits.width);
            output_rows = rows;
            (card_lines, truncated)
        }
        Some(ToolOutput::Skill(skill)) => {
            let body = skill_body(skill, &limits);
            output_links = body.links;
            output_source = body.source;
            (body.lines, body.truncation)
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
        Some(ToolOutput::Answers(answers)) => render_answers(answers, limits.bounded_budget()),
        Some(ToolOutput::WorkflowRun(card)) => {
            let (card_lines, rows) = workflow_card::render(card, limits.width);
            output_rows = rows;
            (card_lines, false)
        }
        Some(ToolOutput::Tasks(tasks)) => {
            let (lines, rows, truncated, links) = task_card::render(
                tasks,
                drawn_script.map(|(_, code)| code.as_str()),
                limits.bounded_budget(),
                limits.width,
            );
            output_rows = rows;
            output_links = links;
            (lines, truncated)
        }
        // Each child owns how much of itself it shows, so the card reports no
        // truncation of its own: there is no one thing for it to open.
        Some(ToolOutput::Batch { entries, .. }) if !entries.is_empty() => {
            let card = render_batch(entries, highlight, &limits);
            output_highlights = card.highlights;
            output_rows = card.rows;
            output_spans = card.spans;
            output_source = card.source;
            output_links = card.links;
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
    if !highlight
        && !output_lines.is_empty()
        && matches!(
            output,
            Some(
                ToolOutput::ReadCode { .. }
                    | ToolOutput::WriteCode { .. }
                    | ToolOutput::Diff { .. }
                    | ToolOutput::Patch { .. }
                    | ToolOutput::GrepResult { .. }
                    | ToolOutput::Index(IndexOutput::File { .. })
                    | ToolOutput::CodeGraph { .. }
                    | ToolOutput::Instructions { .. }
            )
        )
    {
        output_highlights.push(HighlightRegion::new(
            0..output_lines.len(),
            CodeRole::Output,
            &limits,
        ));
    }
    highlights.extend(output_highlights.into_iter().map(|mut region| {
        region.shift(body_start);
        region
    }));
    let mut links = LinkMap::none_for(&lines);
    if output_links.rows.is_empty() {
        output_links = LinkMap::none_for(&output_lines);
    }
    links.rows.extend(output_links.rows);
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
            highlights,
            links,
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
    let wrapped = WrappedRows::new(content.lines, 0, width);
    ToolContent {
        highlights: content
            .highlights
            .into_iter()
            .map(|mut region| {
                region.wrap(&wrapped, width);
                region
            })
            .collect(),
        links: LinkMap {
            rows: content
                .links
                .rows
                .iter()
                .enumerate()
                .flat_map(|(line, links)| wrapped.spans_of(line, links))
                .collect(),
        },
        lines: wrapped.lines(),
        rows: wrapped.expand(content.rows),
        truncation: content.truncation,
        scroll_spans: content
            .scroll_spans
            .into_iter()
            .map(|span| wrapped.scroll_span(span))
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
    use crate::animation::test_clock::FrozenClock;
    use crate::components::tool_display::{AWAITING_APPROVAL, WRITING_COMMAND};
    use crate::markdown::{EXPAND_AFFORDANCE, TRUNCATION_PREFIX};
    use caudra_agent::tools::{
        BATCH_TOOL_NAME, FILE_GREP_TOOL_NAME, FILE_READ_TOOL_NAME, SKILL_TOOL_NAME, ToolEffect,
    };
    use caudra_agent::types::QuestionOption;
    use caudra_agent::{
        ActivityChild, EnvironmentFact, GrepLine, GrepMatchGroup, ShellOutput, SubagentActivity,
        TextOutput,
    };
    use std::{slice::from_ref, time::Duration};
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

    #[test_case(false; "plain")]
    #[test_case(true; "highlighted")]
    fn tabs_keep_local_atomic_ranges_and_unicode_keeps_verbatim_offsets(highlight: bool) {
        const CODE: &str = "\tlet café = \"日本語\";\tprintln!(\"{café}\");";
        let hl = highlight.then(|| caudra_highlight::Highlighter::for_path("test.rs"));
        let rendered = render_code(hl, 1, &[CODE.to_owned()], 1, usize::MAX, CODE_CARD_WIDTH);
        let mut copied = String::new();
        let mut previous = None;
        let mut tabs = 0;
        for (row, line) in rendered.source.rows.iter().zip(&rendered.lines) {
            assert_eq!(row.spans.len(), line.spans.len(), "{SPANS_PER_ROW}");
            for (origin, span) in row.spans.iter().zip(&line.spans) {
                if let SpanSource::Range(origin) = origin {
                    let text = &CODE[origin.range.start as usize..origin.range.end as usize];
                    if origin.verbatim {
                        assert!(text.starts_with(span.content.as_ref()));
                    } else {
                        assert_eq!(text, "\t");
                        tabs += usize::from(previous.as_ref() != Some(&origin.range));
                    }
                    if previous.as_ref() != Some(&origin.range) {
                        copied.push_str(text);
                    }
                    previous = Some(origin.range.clone());
                }
            }
        }
        assert_eq!(tabs, CODE.matches('\t').count());
        assert_eq!(copied, CODE);
        assert_eq!(rendered.source.code[0].source, 0..CODE.len() as u32);
    }

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
        let width = gutter_digits(last) + 1;

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

    const STABLE_GUTTER: &str =
        "a body that grows or opens must not re-gutter the rows already on screen";
    const GUTTER_LEAVES_ROOM: &str =
        "a reserved gutter must still leave a narrow card room to read the code behind it";

    /// Lines long enough to break at every width these cases use, so a gutter
    /// that moved shows as rewrapped rows rather than only as shifted numbers.
    fn wrapping_lines(count: usize) -> String {
        (1..=count)
            .map(|nr| format!("call{nr}(alpha, beta, gamma, delta, epsilon)"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The reported defect: a file arriving line by line widened its gutter as
    /// it crossed a digit boundary, which narrowed the code column and rewrapped
    /// every row that had already been drawn.
    #[test_case(9, 10 ; "nine_to_ten")]
    #[test_case(99, 100 ; "ninety_nine_to_a_hundred")]
    fn a_growing_body_leaves_the_rows_already_drawn_alone(before: usize, after: usize) {
        let drawn: Vec<String> = render_live_body(&wrapping_lines(before), CODE_CARD_WIDTH)
            .iter()
            .map(line_text)
            .collect();

        let grown: Vec<String> = render_live_body(&wrapping_lines(after), CODE_CARD_WIDTH)
            .iter()
            .take(drawn.len())
            .map(line_text)
            .collect();

        assert_eq!(drawn, grown, "{STABLE_GUTTER}");
    }

    /// The same body at two budgets. A gutter sized to the rows on screen
    /// rather than to the file's own range moves the moment a card is opened,
    /// taking every row that was already readable with it.
    #[test]
    fn opening_a_budget_leaves_the_gutter_where_it_was() {
        const SHOWN: usize = 5;
        const TOTAL: usize = 1200;
        let code: Vec<String> = (1..=TOTAL)
            .map(|nr| format!("row {nr} of the file"))
            .collect();

        let closed = render_code(None, 1, &code, TOTAL, SHOWN, CODE_CARD_WIDTH);
        let open = render_code(None, 1, &code, TOTAL, usize::MAX, CODE_CARD_WIDTH);

        let closed: Vec<String> = closed.lines.iter().take(SHOWN).map(line_text).collect();
        let open: Vec<String> = open.lines.iter().take(SHOWN).map(line_text).collect();
        assert_eq!(closed, open, "{STABLE_GUTTER}");
    }

    /// Reserving the gutter is only worth it while the code still has columns
    /// to be read in, which is the thing a narrow terminal has least of.
    #[test_case(40 ; "forty_columns")]
    #[test_case(30 ; "thirty_columns")]
    #[test_case(24 ; "twenty_four_columns")]
    fn a_reserved_gutter_still_leaves_a_narrow_card_room_to_read(width: u16) {
        const CODE: &str = "let total = alpha + beta + gamma + delta;";
        let rendered = render_live_body(CODE, width);

        let rows: Vec<String> = rendered.iter().map(line_text).collect();
        assert!(
            rows.iter()
                .all(|row| row.chars().count() <= usize::from(width)),
            "{GUTTER_LEAVES_ROOM}: {rows:?}"
        );
        for line in &rendered {
            let code = spans_text(&line.spans[1..]);
            assert!(!code.trim().is_empty(), "{GUTTER_LEAVES_ROOM}: {rows:?}");
        }
        for word in ["alpha", "beta", "gamma", "delta"] {
            assert!(
                rows.iter().any(|row| row.contains(word)),
                "{GUTTER_LEAVES_ROOM}: {rows:?}"
            );
        }
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

    const WRAPPED_TRACK_MSG: &str = "a window's track is the rows it painted, so a line the final break split lengthens it \
         instead of leaving the bar counting lines the card no longer has";
    /// Where the window sits in the body behind it. Both are counted in that
    /// body's own lines, which the break does not touch.
    const WINDOWED_BODY_LINES: usize = 40;
    const WINDOWED_BODY_OFFSET: usize = 7;

    /// A window is recorded while the card is still in logical lines, and the
    /// card is broken to its width once, at the end. Whatever that break
    /// lengthened has to be carried across it, or the bar, the wheel and the
    /// painted height stop agreeing about the same rows.
    #[test_case(None; "without_history")]
    #[test_case(Some(WINDOWED_BODY_OFFSET); "with_history")]
    fn a_window_whose_lines_break_keeps_its_track_over_the_rows_it_painted(
        history_start: Option<usize>,
    ) {
        const WINDOW_LINES: usize = 3;
        let over_wide = "z".repeat(usize::from(CODE_CARD_WIDTH) * 2);
        let lines: Vec<Line<'static>> = (0..=WINDOW_LINES)
            .map(|index| Line::from(format!("row{index} {over_wide}")))
            .collect();
        let content = ToolContent {
            highlights: Vec::new(),
            links: LinkMap::none_for(&lines),
            rows: vec![None; lines.len()],
            lines,
            truncation: false,
            // Every line but the first, so the rows it kept are every row the
            // card has once the first line's are taken off the front.
            scroll_spans: Vec::from([ScrollSpan {
                child: None,
                first: 1,
                lines: WINDOW_LINES,
                extent_lines: WINDOW_LINES,
                total: WINDOWED_BODY_LINES,
                offset: WINDOWED_BODY_OFFSET,
                history_start,
            }]),
            source: None,
        };

        let wrapped = wrapped_content(content, CODE_CARD_WIDTH);

        let span = wrapped.scroll_spans[0];
        assert!(
            span.lines > WINDOW_LINES,
            "{WRAPPED_TRACK_MSG}: {} rows for {WINDOW_LINES} lines",
            span.lines
        );
        assert_eq!(
            span.first + span.lines,
            wrapped.lines.len(),
            "{WRAPPED_TRACK_MSG}"
        );
        assert_eq!(
            (span.total, span.offset),
            (WINDOWED_BODY_LINES, WINDOWED_BODY_OFFSET),
            "{WRAPPED_TRACK_MSG}"
        );
        assert_eq!(span.history_start, history_start, "{WRAPPED_TRACK_MSG}");
        assert_eq!(span.extent_lines, WINDOW_LINES, "{WRAPPED_TRACK_MSG}");
        let mut body = ChildBody::traced(wrapped.lines, BodySource::default());
        body.span = Some(span);
        let shifted = body
            .indented(TREE_TRUNK)
            .span
            .expect(WRAPPED_TRACK_MSG)
            .shifted(WINDOW_LINES, Some(0))
            .shift_lines(WINDOW_LINES);
        assert_eq!(
            shifted.first,
            span.first + WINDOW_LINES * 2,
            "{WRAPPED_TRACK_MSG}"
        );
        assert_eq!(shifted.history_start, history_start, "{WRAPPED_TRACK_MSG}");
        assert_eq!(shifted.extent_lines, WINDOW_LINES, "{WRAPPED_TRACK_MSG}");
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
        render_patch(files, false, UNCONSTRAINED_WIDTH)
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

    const RUST_PATCH: &str = "@@ -1,2 +1,2 @@\n fn f() {}\n-let old = 1;\n+let new = 2;\n";
    const CONTEXT: &str = "fn f() {}";
    const HIGHLIGHTED_MSG: &str =
        "a patch hunk is highlighted the way the fragment an edit carries is";
    const FLAG_MSG: &str = "the sync path asks for no highlighting and must not be parsed anyway";
    const BAND_MSG: &str =
        "a changed row wears its diff band under the syntax colour, not instead of it";

    fn highlighted_patch() -> Vec<Line<'static>> {
        render_patch(&one_file(RUST_PATCH), true, UNCONSTRAINED_WIDTH)
    }

    /// An edit's card highlights the fragment it was handed, and a hunk is the
    /// same kind of fragment.
    #[test]
    fn a_patch_hunk_is_highlighted_like_an_edit() {
        assert_eq!(
            diff_fg(&highlighted_patch(), "fn"),
            fg_in_context(FIRST_PATH, "", CONTEXT, "fn"),
            "{HIGHLIGHTED_MSG}"
        );
    }

    /// The card drawn on the UI thread declines the parse to stay cheap, so a
    /// flag quietly ignored would charge every patch for one twice over.
    #[test]
    fn a_patch_honours_the_highlight_flag() {
        let plain = render_patch(&one_file(RUST_PATCH), false, UNCONSTRAINED_WIDTH);
        assert_ne!(
            diff_fg(&plain, "fn"),
            diff_fg(&highlighted_patch(), "fn"),
            "{FLAG_MSG}"
        );
    }

    /// The band is a background the syntax colour sits on. Dropping either half
    /// of that merge still looks plausible on its own, so both are pinned here:
    /// an added row keeps its tint, and its code keeps the colour it would have
    /// had after the context line above it.
    #[test]
    fn a_highlighted_change_keeps_its_diff_band() {
        let lines = highlighted_patch();
        let added = lines
            .iter()
            .find(|line| line.spans[0].content.contains(MARK_ADDED))
            .unwrap_or_else(|| panic!("{BAND_MSG}: {lines:?}"));
        let code = &added.spans[1..];

        assert!(
            code.iter().all(|span| span.style.bg.is_some()),
            "{BAND_MSG}: {added:?}"
        );
        assert_eq!(
            code.iter()
                .find(|span| span.content.contains("let"))
                .and_then(|span| span.style.fg),
            Some(fg_in_context(FIRST_PATH, CONTEXT, "let new = 2;", "let")),
            "{BAND_MSG}: {added:?}"
        );
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

    const Q_HEADER: &str = "Transfer channel";
    const Q_TEXT: &str = "How should bytes move?";
    const Q_PICKED: &str = "Signed URLs";
    const Q_DECLINED: &str = "Base64 in JSON-RPC";
    const Q_DESCRIPTION: &str = "Short-lived URLs instead of bytes";
    const Q_TYPED: &str = "hybrid, but on the same port";
    const UNBOUNDED: usize = usize::MAX;
    const DECLINED_SHOWN: &str = "a declined option is half of what the decision was";

    fn answer(labels: &[&str], options: &[(&str, &str)]) -> Answer {
        Answer {
            header: Q_HEADER.into(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            question: Q_TEXT.into(),
            options: options
                .iter()
                .map(|(label, description)| QuestionOption {
                    label: (*label).to_owned(),
                    description: (*description).to_owned(),
                })
                .collect(),
        }
    }

    fn offered() -> Vec<(&'static str, &'static str)> {
        vec![(Q_PICKED, Q_DESCRIPTION), (Q_DECLINED, "")]
    }

    #[test]
    fn an_answer_card_draws_the_question_and_every_option_it_offered() {
        let (lines, truncated) = render_answers(&[answer(&[Q_PICKED], &offered())], UNBOUNDED);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(!truncated);
        assert!(texts.iter().any(|t| t == Q_HEADER));
        assert!(texts.iter().any(|t| t.contains(Q_TEXT)));
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with(ANSWER_MARK) && t.contains(Q_PICKED))
        );
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with(DECLINED_MARK) && t.contains(Q_DECLINED)),
            "{DECLINED_SHOWN}"
        );
        assert!(texts.iter().any(|t| t.contains(Q_DESCRIPTION)));
    }

    /// The form offers a "type your own answer" row, and what the user typed is
    /// a pick like any other even though no option carries it.
    #[test]
    fn a_typed_answer_is_drawn_as_a_pick_of_its_own() {
        let (lines, _) = render_answers(&[answer(&[Q_TYPED], &offered())], UNBOUNDED);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with(ANSWER_MARK) && t.contains(Q_TYPED))
        );
        assert!(
            texts
                .iter()
                .all(|t| !(t.starts_with(ANSWER_MARK) && t.contains(Q_PICKED))),
            "an offered option the user passed over is not a pick"
        );
    }

    #[test]
    fn a_skipped_question_still_draws_its_form() {
        let (lines, _) = render_answers(&[answer(&[], &offered())], UNBOUNDED);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains(NO_ANSWER)));
        assert!(
            texts.iter().any(|t| t.contains(Q_PICKED)),
            "{DECLINED_SHOWN}"
        );
    }

    /// An answer whose input could not be recovered has no form to draw, and
    /// its picks are what the card has always shown.
    #[test]
    fn an_answer_without_its_form_still_draws_its_picks() {
        let bare = Answer {
            header: Q_HEADER.into(),
            labels: vec![Q_PICKED.into()],
            question: String::new(),
            options: Vec::new(),
        };
        let (lines, _) = render_answers(&[bare], UNBOUNDED);

        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0], Q_HEADER);
        assert!(texts[1].starts_with(ANSWER_MARK) && texts[1].contains(Q_PICKED));
    }

    #[test]
    fn a_form_past_the_budget_is_shortened_and_says_so() {
        let many: Vec<(String, String)> = (0..60)
            .map(|index| (format!("option {index}"), String::new()))
            .collect();
        let borrowed: Vec<(&str, &str)> = many
            .iter()
            .map(|(label, desc)| (label.as_str(), desc.as_str()))
            .collect();
        const BUDGET: usize = 10;

        let (lines, truncated) = render_answers(&[answer(&[], &borrowed)], BUDGET);

        assert!(truncated);
        assert_eq!(lines.len(), BUDGET);
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

    #[test_case("c" ; "c")]
    #[test_case("cpp" ; "cpp")]
    #[test_case("cuda" ; "cuda")]
    #[test_case("objc" ; "objc")]
    #[test_case("cmake" ; "cmake")]
    #[test_case("proto" ; "proto")]
    #[test_case("xml" ; "xml")]
    fn index_language_highlights_with_a_real_syntax(language: &str) {
        let plain_text = &caudra_highlight::syntax_set().find_syntax_plain_text().name;
        let syntax = caudra_highlight::syntax_for_token(index_highlight_token(language));
        assert_ne!(&syntax.name, plain_text);
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

    const REPORT_TOOL: &str = "report_to_parent";
    const REPORT_TITLE: &str = "Validation finding";
    const REPORT_ACK: &str = "Report durably recorded; no reply is expected.";
    const REPORT_ERROR: &str = "Report could not be recorded.";
    const REPORT_DETAIL: &str = "A **useful** detail.";

    #[test_case(BatchToolStatus::Success, "Reported", Some(REPORT_ACK); "success")]
    #[test_case(BatchToolStatus::Error, "Report", Some(REPORT_ERROR); "error")]
    #[test_case(BatchToolStatus::Running, "Reporting", None; "running")]
    #[test_case(BatchToolStatus::Pending, "Report", None; "pending")]
    fn batch_report_cards_preserve_message_and_status(
        status: BatchToolStatus,
        label: &str,
        acknowledgement: Option<&str>,
    ) {
        let entry = BatchToolEntry {
            summary: REPORT_TOOL.into(),
            status,
            raw_input: Some(serde_json::json!({
                "title": REPORT_TITLE,
                "message": REPORT_DETAIL,
                "blocked": true,
            })),
            output: acknowledgement.map(|text| ToolOutput::Plain(text.into())),
            ..batch_entry(REPORT_TOOL, 0)
        };
        for compact in [false, true] {
            for open in [false, true] {
                let limits = limits(BatchViews::new(open.then_some(0)))
                    .with_width(200)
                    .with_policy(
                        CardPolicy {
                            compact,
                            ..CardPolicy::default()
                        },
                        Arc::default(),
                    );
                let card = render_batch(from_ref(&entry), false, &limits);
                let text = card
                    .lines
                    .iter()
                    .map(|line| spans_text(&line.spans))
                    .collect::<String>();
                assert!(text.contains(&format!("{label} Blocked")), "{text}");
                assert!(text.contains(REPORT_TITLE), "{text}");
                assert!(!text.contains(REPORT_TOOL), "{text}");
                assert!(!text.contains("message="), "{text}");
                assert!(!text.contains("title="), "{text}");
                assert!(!text.contains("blocked="), "{text}");
                if open {
                    assert!(text.contains("useful"), "{text}");
                    assert!(!text.contains(REPORT_DETAIL), "{text}");
                    if let Some(acknowledgement) = acknowledgement {
                        assert_eq!(text.matches(acknowledgement).count(), 1, "{text}");
                    }
                } else if compact {
                    assert!(!text.contains("useful"), "{text}");
                    assert!(text.contains(BATCH_FOLDED_MARK), "{text}");
                }
                if status != BatchToolStatus::Success {
                    assert!(!text.contains("Reported"), "{text}");
                    assert!(!text.contains(REPORT_ACK), "{text}");
                }
                assert_eq!(card.rows.len(), card.lines.len());
                assert_eq!(card.links.rows.len(), card.lines.len());
            }
        }
    }

    #[test_case(24; "narrow")]
    #[test_case(200; "wide")]
    fn batch_report_cards_recover_legacy_message(width: u16) {
        let entry = BatchToolEntry {
            summary: REPORT_TOOL.into(),
            raw_input: Some(serde_json::json!({
                "message": format!("\n\n{REPORT_TITLE}\n\n{REPORT_DETAIL}\n\u{1b}[31m"),
            })),
            output: Some(ToolOutput::Plain(REPORT_ACK.into())),
            ..batch_entry(REPORT_TOOL, 0)
        };
        let limits = limits(BatchViews::new([0])).with_width(width);
        let card = render_batch(&[entry], false, &limits);
        let text = card
            .lines
            .iter()
            .map(|line| spans_text(&line.spans))
            .collect::<String>();
        assert!(!text.contains(REPORT_TOOL), "{text}");
        assert!(!text.chars().any(char::is_control));
        assert_eq!(card.rows.len(), card.lines.len());
        assert_eq!(card.links.rows.len(), card.lines.len());
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
        let limits = limits(BatchViews::new([0])).with_width(width + batch_child_indent_width());
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

    const SKILL_LOCATION: &str = "builtin:herdr";
    const SKILL_HEADING: &str = "Herdr";
    const SKILL_PROSE: &str = "Drive panes.";

    /// A child that loaded a skill draws what the skill's own card draws: the
    /// file it came from, then the document rather than the numbered text the
    /// model reads.
    #[test]
    fn a_skill_child_draws_its_document_under_its_location() {
        let entry = BatchToolEntry {
            output: Some(ToolOutput::Skill(SkillOutput {
                location: SKILL_LOCATION.into(),
                body: format!("# {SKILL_HEADING}\n\n{SKILL_PROSE}"),
            })),
            ..batch_entry(SKILL_TOOL_NAME, 0)
        };
        let card = render_batch(from_ref(&entry), false, &limits(BatchViews::new([0])));

        let body = lone_child_body(&card);
        let rows: Vec<&str> = body.iter().map(|(text, _)| text.trim()).collect();
        assert_eq!(
            rows.iter().take(2).copied().collect::<Vec<_>>(),
            [SKILL_LOCATION, SKILL_HEADING],
            "{rows:#?}"
        );
        assert!(rows.contains(&SKILL_PROSE), "{rows:#?}");
        assert!(all_indented(&body), "{CHILD_INDENT_MSG}: {rows:#?}");
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
        let limits = limits(BatchViews::default())
            .with_width(NARROW_BODY_WIDTH + batch_child_indent_width());

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
            vec![Some(RowTarget::Item(0)); title.len()],
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
            limits(BatchViews::new([0])).with_width(NARROW_BODY_WIDTH + batch_child_indent_width());
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
            limits(BatchViews::new([0])).with_width(NARROW_BODY_WIDTH + batch_child_indent_width());
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

    const CHILD_SPAN_MSG: &str = "a child drawn in a window publishes it whether or not there is a footer to draw, or a \
         child whose body fits has a window with no bar and hands the wheel back to the transcript";
    const TREE_LEVEL_MSG: &str = "every connector draws one tree level, so a child body narrowed by one indent lines up \
         under whichever of them drew it";

    /// A window with nothing either side of it writes no footer, and the span
    /// was published only alongside one. The bar is what makes a child's rows
    /// the wheel's target, so dropping it costs the wheel to the transcript
    /// for exactly the bodies that already fit.
    #[test]
    fn a_child_body_that_fits_its_window_still_publishes_it() {
        const BODY: &str = "first\nsecond\nthird";
        let (lines, source) = plain_body(BODY, NARROW_BODY_WIDTH);
        let painted = lines.len();
        let limits = limits(BatchViews::default()).with_scroll(Some(ScrollWindow {
            height: painted + 1,
            offset: 0,
            follow: true,
        }));

        let body = child_view(
            ChildBody::traced(lines, source),
            limits.scroll,
            limits.budget,
            ScrollTail::Settled,
            "",
        );

        let span = body.span.expect(CHILD_SPAN_MSG);
        assert_eq!(
            (span.first, span.lines, span.total, span.offset),
            (0, painted, painted, 0),
            "{CHILD_SPAN_MSG}"
        );
        assert_eq!(body.lines.len(), painted, "{CHILD_SPAN_MSG}");
        assert_eq!(span.history_start, None, "{CHILD_SPAN_MSG}");
        assert!(!body.truncation, "{CHILD_SPAN_MSG}");
    }

    /// `RenderLimits::child` narrows a body by one indent and `indent_all`
    /// puts one back in front of every row. The two are the same number only
    /// while every glyph that can open that indent measures the same width,
    /// and half the tree is drawn in box glyphs whose bytes say otherwise.
    #[test_case(TREE_BRANCH ; "branch")]
    #[test_case(TREE_LAST ; "last")]
    #[test_case(TREE_TRUNK ; "trunk")]
    #[test_case(TREE_GAP ; "gap")]
    fn every_tree_glyph_draws_one_child_indent(connector: &str) {
        assert_eq!(
            UnicodeWidthStr::width(connector),
            UnicodeWidthStr::width(TREE_GAP),
            "{TREE_LEVEL_MSG}"
        );
        assert_eq!(
            UnicodeWidthStr::width(format!("{connector}{BATCH_BODY_PAD}").as_str()),
            usize::from(batch_child_indent_width()),
            "{TREE_LEVEL_MSG}"
        );
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
            vec![RowTarget::Item(0), RowTarget::Item(1)],
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
            vec![
                RowTarget::Item(0),
                RowTarget::Item(0),
                RowTarget::Item(0),
                RowTarget::Item(1)
            ],
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
        assert_eq!(
            unique_targets(&rows),
            vec![RowTarget::Item(0), RowTarget::Item(1)]
        );
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
                .filter(|row| **row == Some(RowTarget::Item(1)))
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
            unique_targets(&rows).contains(&RowTarget::Item(0)),
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

    /// What the reader's window is configured at here: short enough that a
    /// body twice its height leaves scrollback behind it, tall enough that a
    /// windowed body is unmistakable beside a folded one.
    const NESTED_WINDOW_LINES: u32 = 4;
    const NESTED_BODY_LINES: usize = NESTED_WINDOW_LINES as usize * 2;
    const NESTED_CHILD: &str = TASK_TOOL_NAME;
    const NESTED_RUNNING: &str = "Delegating";
    const NESTED_SETTLED: &str = "Delegated";
    const ONE_BODY_MSG: &str = "one nesting path draws one scrolling body, so a row gained at the bottom scrolls inside \
         it instead of growing every level above it";
    const NESTED_ROSTER_MSG: &str =
        "every level keeps one row per child it dispatched, in the tense that child is in";
    const BODY_MOVES_MSG: &str =
        "opening a nested row hands the path's one body down to the level it names";

    /// A dispatch rather than a read: what it did is its own call, so it is
    /// drawn rather than folded to its row, which is what gives it a body for
    /// the one-body rule to place.
    fn dispatch_entry(tool: &str, body_lines: usize) -> BatchToolEntry {
        BatchToolEntry {
            effect: ToolEffect::Orchestrator,
            ..batch_entry(tool, body_lines)
        }
    }

    fn nested_batch_entry(children: Vec<BatchToolEntry>) -> BatchToolEntry {
        BatchToolEntry {
            output: Some(ToolOutput::Batch {
                entries: children,
                text: String::new(),
            }),
            ..dispatch_entry(BATCH_TOOL_NAME, 0)
        }
    }

    fn scrolling_limits(views: BatchViews) -> RenderLimits {
        limits(views).with_policy(
            CardPolicy {
                scroll_card_lines: NESTED_WINDOW_LINES,
                ..CardPolicy::default()
            },
            Arc::default(),
        )
    }

    fn nested_card(children: Vec<BatchToolEntry>, views: BatchViews) -> BatchCard {
        render_batch(
            &[nested_batch_entry(children)],
            false,
            &scrolling_limits(views),
        )
    }

    /// The level that has nothing above it answers exactly what it answered
    /// before: the one-body rule is about nesting, and there is none here.
    #[test]
    fn a_single_level_batch_keeps_its_window() {
        let card = render_batch(
            &[dispatch_entry(NESTED_CHILD, NESTED_BODY_LINES)],
            false,
            &scrolling_limits(BatchViews::default()),
        );

        assert_eq!(
            body_count(&card.lines),
            NESTED_WINDOW_LINES as usize,
            "{ONE_BODY_MSG}"
        );
    }

    /// The jumping this exists to stop: the dispatch under a nested batch
    /// opened a window of its own inside a body that was already being drawn,
    /// so every row it gained pushed both levels above it taller.
    #[test]
    fn a_nested_dispatch_opens_no_second_body_under_its_parent() {
        let card = nested_card(
            Vec::from([dispatch_entry(NESTED_CHILD, NESTED_BODY_LINES)]),
            BatchViews::default(),
        );

        assert_eq!(body_count(&card.lines), 0, "{ONE_BODY_MSG}");
        assert!(
            card.lines
                .iter()
                .any(|line| line_text(line).contains(BATCH_FOLDED_MARK)),
            "{BODY_MOVES_MSG}: the row the body moved off still offers it"
        );
    }

    /// Losing the body must not cost the row. The nested level keeps its own
    /// node, its sigil and the verb its status puts that node in, and the verb
    /// follows the call as it runs.
    #[test_case(BatchToolStatus::Running, NESTED_RUNNING ; "still_running")]
    #[test_case(BatchToolStatus::Success, NESTED_SETTLED ; "answered")]
    fn a_nested_dispatch_keeps_its_row_and_its_status(status: BatchToolStatus, label: &str) {
        let card = nested_card(
            Vec::from([
                BatchToolEntry {
                    status,
                    ..dispatch_entry(NESTED_CHILD, NESTED_BODY_LINES)
                },
                dispatch_entry(NESTED_CHILD, NESTED_BODY_LINES),
            ]),
            BatchViews::default(),
        );

        let drawn: Vec<String> = card.lines.iter().map(line_text).collect();
        assert_eq!(drawn.len(), 3, "{NESTED_ROSTER_MSG}: {drawn:?}");
        assert!(drawn[1].contains(label), "{NESTED_ROSTER_MSG}: {drawn:?}");
        assert!(
            drawn[2].contains(NESTED_SETTLED),
            "{NESTED_ROSTER_MSG}: {drawn:?}"
        );
    }

    /// The body belongs to the deepest level the reader opened, and the tree
    /// it moved through is the same tree either way.
    #[test]
    fn opening_a_nested_row_moves_the_body_down_to_it() {
        let child = || Vec::from([dispatch_entry(NESTED_CHILD, NESTED_BODY_LINES)]);
        let folded = nested_card(child(), BatchViews::default());
        let opened = nested_card(child(), BatchViews::new([0]));

        assert_eq!(body_count(&folded.lines), 0, "{ONE_BODY_MSG}");
        assert_eq!(
            body_count(&opened.lines),
            NESTED_WINDOW_LINES as usize,
            "{BODY_MOVES_MSG}"
        );
        let roster = |card: &BatchCard| {
            card.lines
                .iter()
                .filter(|line| line_text(line).contains(NESTED_SETTLED))
                .count()
        };
        assert_eq!(roster(&folded), roster(&opened), "{NESTED_ROSTER_MSG}");
    }

    fn nested_height(leaf_lines: usize, limits: &RenderLimits) -> usize {
        render_batch(
            &[nested_batch_entry(Vec::from([dispatch_entry(
                NESTED_CHILD,
                leaf_lines,
            )]))],
            false,
            limits,
        )
        .lines
        .len()
    }

    /// The leaf's own budget bounds it eventually, but not before it has grown
    /// through every level between it and the card. With the body placed once,
    /// what it prints moves nothing at all.
    #[test]
    fn growth_below_a_folded_nesting_moves_nothing_above_it() {
        let limits = limits(BatchViews::default());

        assert_eq!(
            nested_height(NESTED_BODY_LINES, &limits),
            nested_height(NESTED_BODY_LINES * 8, &limits),
            "{ONE_BODY_MSG}"
        );
    }

    /// And once the reader has opened the nesting, the window they opened is
    /// where the growth lands: past its cap the leaf scrolls instead of
    /// pushing the levels above it down.
    #[test]
    fn growth_past_the_one_windows_cap_moves_nothing_above_it() {
        let limits = scrolling_limits(BatchViews::new([0]));

        assert_eq!(
            nested_height(NESTED_BODY_LINES * 4, &limits),
            nested_height(NESTED_BODY_LINES * 8, &limits),
            "{ONE_BODY_MSG}"
        );
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
    const CHILD_PROJECT: &str = "/project";

    /// The row without the tree column, which every child carries and no
    /// assertion here is about. Drawn in a session directory, as every live
    /// roster is.
    fn child_row(entry: BatchToolEntry) -> String {
        let in_project =
            limits(BatchViews::default()).with_cwd(Some(Arc::from(Path::new(CHILD_PROJECT))));
        let row = line_text(&render_batch(&[entry], false, &in_project).lines[0]);
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

    const STAGED_CHILD_MSG: &str = "a child that has not run names the stage it is in, and only \
        one that is written and waiting on nothing but its turn is queued";

    #[test_case(BatchToolStatus::Drafting, WRITING_COMMAND, false ; "a_command_being_written")]
    #[test_case(BatchToolStatus::Pending, "Run", true ; "a_written_command_is_queued")]
    #[test_case(
        BatchToolStatus::AwaitingApproval,
        AWAITING_APPROVAL,
        false
        ; "a_command_awaiting_approval"
    )]
    fn a_staged_child_row_names_its_stage(status: BatchToolStatus, label: &str, queued: bool) {
        let row = child_row(BatchToolEntry {
            status,
            output: None,
            ..batch_entry(SHELL_CHILD, 0)
        });
        assert!(
            row.starts_with(&format!("$ {label} ")),
            "{STAGED_CHILD_MSG}: {row:?}"
        );
        assert_eq!(
            row.contains(QUEUED_ANNOTATION),
            queued,
            "{STAGED_CHILD_MSG}: {row:?}"
        );
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

    const STREAMED_SCRIPT_MSG: &str = "a child still having its call written draws the whole \
        script that has arrived, and its row stops repeating the first line the body carries";
    const STREAMED_COMMAND: &str = "set -e\ncargo build --workspace\ncargo test --workspace";

    /// A child mid-stream: its script has arrived, nothing has been dispatched,
    /// and so there is no output and no live tail to draw instead.
    fn streaming_script_entry() -> BatchToolEntry {
        BatchToolEntry {
            status: BatchToolStatus::Pending,
            summary: STREAMED_COMMAND.lines().next().unwrap_or_default().into(),
            input: Some(ToolInput::Code {
                language: "bash".into(),
                code: STREAMED_COMMAND.into(),
            }),
            output: None,
            ..batch_entry(SHELL_CHILD, 0)
        }
    }

    #[test]
    fn a_streaming_childs_script_is_drawn_whole() {
        let card = render_batch(
            &[streaming_script_entry()],
            false,
            &limits(BatchViews::default()),
        );

        let drawn: Vec<String> = card.lines.iter().map(line_text).collect();
        for line in STREAMED_COMMAND.lines() {
            assert!(
                drawn.iter().any(|row| row.contains(line)),
                "{STREAMED_SCRIPT_MSG}: {line:?} missing from {drawn:?}"
            );
        }
    }

    #[test]
    fn a_streaming_childs_row_does_not_repeat_its_scripts_first_line() {
        let card = render_batch(
            &[streaming_script_entry()],
            false,
            &limits(BatchViews::default()),
        );

        let first = STREAMED_COMMAND.lines().next().unwrap_or_default();
        let carrying = card
            .lines
            .iter()
            .filter(|line| line_text(line).contains(first))
            .count();
        assert_eq!(carrying, 1, "{STREAMED_SCRIPT_MSG}");
    }

    const STREAMED_PROMPT_MSG: &str = "a generating child draws the prompt that has arrived under \
        a row still naming the file it writes: the two are different things, so both are shown";
    const STREAMED_PROMPT: &str =
        "A wide cinematic shot of a lighthouse\nat dusk, storm clouds behind it";
    const GENERATED_PATH: &str = "assets/hero.png";
    const IMAGE_CHILD: &str = "image_generate";

    /// The reported bug: a generation inside a batch showed a bare path and
    /// nothing else until the image came back, which is the longest wait of
    /// any call the roster can hold.
    #[test]
    fn a_streaming_generations_prompt_hangs_under_a_row_naming_its_file() {
        let entry = BatchToolEntry {
            status: BatchToolStatus::Pending,
            summary: GENERATED_PATH.into(),
            input: Some(ToolInput::Code {
                language: "markdown".into(),
                code: STREAMED_PROMPT.into(),
            }),
            output: None,
            ..batch_entry(IMAGE_CHILD, 0)
        };

        let card = render_batch(&[entry], false, &limits(BatchViews::default()));

        let drawn: Vec<String> = card.lines.iter().map(line_text).collect();
        assert!(
            drawn[0].contains(GENERATED_PATH),
            "{STREAMED_PROMPT_MSG}: {drawn:?}"
        );
        for line in STREAMED_PROMPT.lines() {
            assert!(
                drawn[1..].iter().any(|row| row.contains(line)),
                "{STREAMED_PROMPT_MSG}: {line:?} missing from {drawn:?}"
            );
        }
    }

    const SETTLED_MSG: &str = "a settled child owns its row and its body and nothing else: a \
        tally hung off it as a node leaves the body drawn to the left of that node";

    /// The reported bug: a finished `task` child drew `└── 2.9s` as a node of
    /// the tree and then its answer beneath at a shallower indent, so reading
    /// down one child stepped back out a level.
    #[test]
    fn a_settled_child_carries_its_tally_on_its_own_row() {
        let _clock = FrozenClock::at(Duration::ZERO);
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
            .filter(|(_, row)| **row == Some(RowTarget::Item(0)))
            .map(|(line, _)| line_text(line))
            .collect();
        let tally = SubagentProgress::tally(BATCHING_TOOLS, Duration::ZERO);
        assert!(owned[0].contains(&tally), "{SETTLED_MSG}: {owned:?}");
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

    const HISTORY_WINDOW: u32 = 4;
    const HISTORY_WIDTH: u16 = 80;
    const HISTORY_CALL: &str = "history call";
    const HISTORY_CALL_COUNT: usize = 8;
    const HISTORY_WINDOW_MSG: &str = "a task windows output and retained activity rows together";
    const HISTORY_REACHABLE_MSG: &str = "every retained row remains reachable in the task window";
    const HISTORY_COLLAPSED_MSG: &str = "folding output must not hide the task's status tree";
    const HISTORY_HEIGHTS: [usize; 5] = [3, 4, 6, 6, 6];

    fn history_report(batch: usize, count: usize) -> SubagentProgress {
        SubagentProgress {
            activity: SubagentActivity::batch(
                Arc::from(BATCH_TOOL_NAME),
                &format!("{count} tools"),
                (0..count)
                    .map(|index| {
                        activity_child(
                            SHELL_CHILD,
                            &format!("{HISTORY_CALL} {batch}-{index}"),
                            BatchToolStatus::Running,
                        )
                    })
                    .collect(),
            ),
            tools: batch as u32,
            elapsed: Duration::ZERO,
        }
    }

    fn retained_progress() -> ToolProgress {
        let mut progress = ToolProgress::live(history_report(0, 1));
        progress.update(SubagentProgress {
            activity: SubagentActivity::Thinking { title: None },
            ..progress.report.clone()
        });
        progress.update(history_report(1, 5));
        progress.update(SubagentProgress {
            activity: SubagentActivity::Thinking { title: None },
            ..progress.report.clone()
        });
        progress.update(history_report(2, 2));
        progress
    }

    fn history_card(progress: &ToolProgress, limits: RenderLimits) -> BatchCard {
        render_batch(
            &[BatchToolEntry {
                status: BatchToolStatus::Running,
                output: None,
                ..batch_entry(TASK_TOOL_NAME, 0)
            }],
            false,
            &RenderLimits {
                progress: Arc::new(HashMap::from([(0, progress.clone())])),
                ..limits
            },
        )
    }

    fn history_text(line: &Line<'static>) -> String {
        line_text(line)
            .split(CHILD_ACTIVITY_SEPARATOR)
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn retained_batches_fill_one_task_window_across_thinking_phases() {
        let mut progress = batching_report(Vec::new());
        let limits = limits(BatchViews::default())
            .with_policy(
                CardPolicy {
                    scroll_card_lines: HISTORY_WINDOW,
                    ..CardPolicy::default()
                },
                Arc::default(),
            )
            .with_width(HISTORY_WIDTH);
        let mut heights = Vec::new();
        for (batch, count) in [Some(1), None, Some(5), None, Some(2)]
            .into_iter()
            .enumerate()
        {
            progress.update(match count {
                Some(count) => history_report(batch, count),
                None => SubagentProgress {
                    activity: SubagentActivity::Thinking { title: None },
                    ..progress.report.clone()
                },
            });
            let card = history_card(&progress, limits.clone());
            heights.push(card.lines.len());
            assert_eq!(card.spans.len(), 1, "{HISTORY_WINDOW_MSG}");
            assert_eq!(card.spans[0].history_start, Some(0), "{HISTORY_WINDOW_MSG}");
            assert!(
                card.rows.iter().all(|row| *row == Some(RowTarget::Item(0))),
                "{HISTORY_WINDOW_MSG}"
            );
            assert!(
                card.lines
                    .iter()
                    .all(|line| !line_text(line).trim().is_empty()),
                "{HISTORY_WINDOW_MSG}"
            );
        }
        assert_eq!(heights, HISTORY_HEIGHTS, "{HISTORY_WINDOW_MSG}");
    }

    #[test_case(48; "narrow")]
    #[test_case(HISTORY_WIDTH; "wide")]
    fn output_and_every_retained_row_share_the_child_scroll_span(width: u16) {
        let progress = retained_progress();
        let limits = limits(BatchViews::new([0]))
            .with_width(width)
            .with_progress(
                Arc::default(),
                Arc::new(HashMap::from([(0, LONG_PARAGRAPH.to_owned())])),
                Arc::default(),
            );
        let whole = history_card(&progress, limits.clone());
        let expected: Vec<_> = whole.lines.iter().skip(1).map(history_text).collect();
        let history_start = plain_body(
            LONG_PARAGRAPH,
            width.saturating_sub(batch_child_indent_width()),
        )
        .0
        .len();
        assert!(whole.spans.is_empty(), "{HISTORY_REACHABLE_MSG}");
        assert_eq!(
            expected
                .iter()
                .filter(|line| line.contains(HISTORY_CALL))
                .count(),
            HISTORY_CALL_COUNT,
            "{HISTORY_REACHABLE_MSG}"
        );
        for offset in 0..expected.len() {
            let window = ScrollWindow {
                height: HISTORY_WINDOW as usize,
                offset,
                follow: false,
            };
            let (start, end) = window.range(expected.len());
            let card = history_card(
                &progress,
                limits.clone().with_policy(
                    CardPolicy {
                        scroll_card_lines: HISTORY_WINDOW,
                        ..CardPolicy::default()
                    },
                    Arc::new(HashMap::from([(0, window)])),
                ),
            );
            assert_eq!(card.spans.len(), 1, "{HISTORY_WINDOW_MSG}");
            let span = card.spans[0];
            assert_eq!(span.extent_lines, span.lines, "{HISTORY_WINDOW_MSG}");
            assert_eq!(
                span.history_start,
                Some(history_start),
                "{HISTORY_WINDOW_MSG}"
            );
            assert_eq!(
                (span.child, span.first, span.lines, span.total, span.offset),
                (Some(0), 1, end - start, expected.len(), start),
                "{HISTORY_WINDOW_MSG}"
            );
            let shown: Vec<_> = card.lines[span.first..span.first + span.lines]
                .iter()
                .map(history_text)
                .collect();
            assert_eq!(shown, expected[start..end], "{HISTORY_REACHABLE_MSG}");
            assert!(
                card.rows.iter().all(|row| *row == Some(RowTarget::Item(0))),
                "{HISTORY_WINDOW_MSG}"
            );
            let source = card.source.as_ref().expect(ROWS_PER_CARD_LINE);
            assert_eq!(source.rows.len(), card.lines.len(), "{ROWS_PER_CARD_LINE}");
            for (row, line) in source.rows.iter().zip(&card.lines) {
                assert_eq!(row.spans.len(), line.spans.len(), "{SPANS_PER_ROW}");
            }
        }
    }

    #[test_case(false, false, false, true; "streaming_output_opens")]
    #[test_case(true, false, false, false; "compact_keeps_only_status")]
    #[test_case(false, true, false, false; "always_collapsed_keeps_only_status")]
    #[test_case(false, false, true, false; "ancestor_body_keeps_only_status")]
    fn task_history_respects_the_existing_output_expansion_policy(
        compact: bool,
        always_collapsed: bool,
        body_taken: bool,
        shows_output: bool,
    ) {
        let progress = retained_progress();
        let card = history_card(
            &progress,
            RenderLimits {
                body_taken,
                live: Arc::new(HashMap::from([(0, CHILD_BODY.to_owned())])),
                ..limits(BatchViews::default()).with_policy(
                    CardPolicy {
                        compact,
                        always_collapsed: if always_collapsed {
                            Arc::from([TASK_TOOL_NAME.to_owned()])
                        } else {
                            Arc::default()
                        },
                        ..CardPolicy::default()
                    },
                    Arc::default(),
                )
            },
        );
        assert_eq!(
            body_count(&card.lines) > 0,
            shows_output,
            "{HISTORY_COLLAPSED_MSG}"
        );
        assert_eq!(
            card.lines
                .iter()
                .filter(|line| line_text(line).contains(HISTORY_CALL))
                .count(),
            HISTORY_CALL_COUNT,
            "{HISTORY_REACHABLE_MSG}"
        );
        assert!(card.spans.is_empty(), "{HISTORY_REACHABLE_MSG}");
    }

    const NESTED_HEIGHT: &str = "a nested roster must keep its height as the subagent moves \
        between calls, or every switch reflows the rows under it";
    /// Wider than the card and much wider than the call beside it: the pair a
    /// subagent switching between them used to resize the tree with.
    const LONG_CALL: &str =
        "cargo nextest run --workspace --locked --no-fail-fast --status-level all";
    const SHORT_CALL: &str = "fn main";
    const NESTED_WIDTH: u16 = 48;

    /// The reported bug: the nested rows wrapped, so their height tracked
    /// whichever call the subagent had reached, and a batch of subagents each
    /// working through tools of different lengths resized the tree under the
    /// reader continuously.
    #[test]
    fn a_nested_roster_keeps_its_height_across_calls() {
        let entries = [
            batch_entry(TASK_TOOL_NAME, 0),
            batch_entry(TASK_TOOL_NAME, 1),
        ];
        let height = |call: &str| {
            let roster = vec![
                activity_child(SHELL_CHILD, call, BatchToolStatus::Running),
                activity_child(FILE_GREP_TOOL_NAME, call, BatchToolStatus::Pending),
            ];
            let mut limits = limits(BatchViews::default()).with_width(NESTED_WIDTH);
            limits.progress = Arc::new(HashMap::from([(0, batching_report(roster))]));
            let card = render_batch(&entries, false, &limits);
            // The card is broken to its width at the end, so the rows the
            // reader sees are the wrapped ones, not the lines built here.
            WrappedRows::new(card.lines, 0, NESTED_WIDTH)
                .per_line
                .iter()
                .map(Vec::len)
                .sum::<usize>()
        };

        assert_eq!(height(SHORT_CALL), height(LONG_CALL), "{NESTED_HEIGHT}");
    }

    /// The reported bug: a dispatched subagent running its own batch said only
    /// `Batching 2 tools`, and what it was batching was invisible. It is a
    /// third level of the same tree, carrying the trunk of the child it
    /// belongs to.
    #[test]
    fn a_batching_child_draws_its_roster_as_a_third_level() {
        let _clock = FrozenClock::at(Duration::ZERO);
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
                format!(
                    "{TREE_BRANCH}\u{2756} Delegated {TASK_TOOL_NAME} summary ({})",
                    SubagentProgress::tally(BATCHING_TOOLS, Duration::ZERO)
                ),
                format!("{TREE_TRUNK}{TREE_LAST}\u{21f6} Batching {BATCHING_SUMMARY}"),
                format!("{TREE_TRUNK}{TREE_GAP}{TREE_BRANCH}$ Running cargo check"),
                format!("{TREE_TRUNK}{TREE_GAP}{TREE_LAST}⌕ Grepped fn main"),
            ],
            "{TREE_MSG}"
        );
        assert!(
            card.rows[..4]
                .iter()
                .all(|row| *row == Some(RowTarget::Item(0))),
            "every row of a child answers for the child that owns it"
        );
    }

    /// The calls one roster names, distinct enough that a row drawn in the
    /// wrong place reads as the wrong call rather than as a changed one.
    const ROSTER_CALLS: [&str; 3] = ["cargo check", "cargo test", "cargo clippy"];
    /// The rows a dispatched child draws above its own roster: its summary row
    /// and the activity row the roster hangs off.
    const ROSTER_HEAD: usize = 2;
    /// The roster row every state-change case moves.
    const MOVED_ROW: usize = ROSTER_HEAD + 1;
    const QUEUED_ROW: &str = "Run";
    const ROSTER_NAMED_MSG: &str =
        "a roster names every child the call has, including the ones still queued behind it";
    const ROSTER_HELD_MSG: &str = "a child changing state rewrites its own row and leaves the \
        roster the height and the order it already had";
    const ROSTER_GROWS_MSG: &str = "a call still spelling its children out has not named them all \
        yet, so its roster is still free to grow";

    /// The card a dispatched child's batch report draws, with its children in
    /// `states`. The clock is held because the child's row carries its tally,
    /// and two rosters drawn a render apart must not differ by the time between.
    fn nested_roster(states: [BatchToolStatus; ROSTER_CALLS.len()]) -> Vec<String> {
        let _clock = FrozenClock::at(Duration::ZERO);
        let children = ROSTER_CALLS
            .iter()
            .zip(states)
            .map(|(call, status)| activity_child(SHELL_CHILD, call, status))
            .collect();
        let mut limits = limits(BatchViews::default());
        limits.progress = Arc::new(HashMap::from([(0, batching_report(children))]));
        render_batch(&[batch_entry(TASK_TOOL_NAME, 0)], false, &limits)
            .lines
            .iter()
            .map(line_text)
            .collect()
    }

    /// The row `index` draws in a nested roster. The prefix is positional, so
    /// the verb is the only part a state change is allowed to move.
    fn nested_row(index: usize, verb: &str) -> String {
        let connector = match index + 1 == ROSTER_CALLS.len() {
            true => TREE_LAST,
            false => TREE_BRANCH,
        };
        format!(
            "{TREE_GAP}{TREE_GAP}{connector}$ {verb} {}",
            ROSTER_CALLS[index]
        )
    }

    /// The reservation that costs nothing: the call named all three children
    /// when it parsed, so all three have a row before any of them runs.
    #[test]
    fn a_nested_roster_names_every_child_before_any_of_them_starts() {
        let rows = nested_roster([BatchToolStatus::Pending; ROSTER_CALLS.len()]);

        assert_eq!(
            rows[ROSTER_HEAD..],
            [
                nested_row(0, QUEUED_ROW),
                nested_row(1, QUEUED_ROW),
                nested_row(2, QUEUED_ROW),
            ],
            "{ROSTER_NAMED_MSG}: {rows:?}"
        );
    }

    #[test_case(BatchToolStatus::Running, "Running" ; "started")]
    #[test_case(BatchToolStatus::Success, "Ran"     ; "answered")]
    #[test_case(BatchToolStatus::Error,   QUEUED_ROW ; "failed")]
    fn a_nested_child_changing_state_rewrites_its_row_alone(status: BatchToolStatus, verb: &str) {
        let queued = nested_roster([BatchToolStatus::Pending; ROSTER_CALLS.len()]);
        let moved = nested_roster([BatchToolStatus::Pending, status, BatchToolStatus::Pending]);
        let siblings = |rows: &[String]| {
            rows.iter()
                .enumerate()
                .filter(|(row, _)| *row != MOVED_ROW)
                .map(|(_, text)| text.clone())
                .collect::<Vec<_>>()
        };

        assert_eq!(moved.len(), queued.len(), "{ROSTER_HELD_MSG}: {moved:?}");
        assert_eq!(
            moved[MOVED_ROW],
            nested_row(1, verb),
            "{ROSTER_HELD_MSG}: {moved:?}"
        );
        assert_eq!(siblings(&moved), siblings(&queued), "{ROSTER_HELD_MSG}");
    }

    /// A child that answers before the ones dispatched with it keeps the row
    /// it was dispatched into, so nothing under it slides up a line.
    #[test]
    fn a_nested_child_answering_early_leaves_its_siblings_in_place() {
        let rows = nested_roster([
            BatchToolStatus::Pending,
            BatchToolStatus::Pending,
            BatchToolStatus::Success,
        ]);

        assert_eq!(
            rows[ROSTER_HEAD..],
            [
                nested_row(0, QUEUED_ROW),
                nested_row(1, QUEUED_ROW),
                nested_row(2, "Ran"),
            ],
            "{ROSTER_HELD_MSG}: {rows:?}"
        );
    }

    /// A roster row before the child behind it has anything to show: named, in
    /// a state, and with no output of its own.
    fn queued_entry(summary: &str, status: BatchToolStatus) -> BatchToolEntry {
        BatchToolEntry {
            summary: summary.to_owned(),
            status,
            output: None,
            ..batch_entry(SHELL_CHILD, 0)
        }
    }

    fn single_level_rows(states: &[BatchToolStatus]) -> Vec<String> {
        let entries: Vec<BatchToolEntry> = ROSTER_CALLS
            .iter()
            .zip(states)
            .map(|(call, status)| queued_entry(call, *status))
            .collect();
        render_batch(&entries, false, &limits(BatchViews::default()))
            .lines
            .iter()
            .map(line_text)
            .collect()
    }

    const LIVE_HEADER_PATH: &str = "src/abcdefghijklmnopq.rs";
    const LIVE_HEADER_SIBLING: &str = "sibling";
    const LIVE_HEADER_WHOLE: &str = "a batch child's heading breaks to as many rows as its summary needs, whatever the \
         call is doing, or a command is unreadable for as long as it is worth watching";
    const LIVE_HEADER_BROKE: &str =
        "the fixture must be narrow enough to break the summary, or it proves nothing";

    fn batch_header_frame(status: BatchToolStatus, nested: bool) -> ToolContent {
        let mut entries = vec![
            BatchToolEntry {
                tool: FILE_READ_TOOL_NAME.into(),
                ..queued_entry(LIVE_HEADER_PATH, status)
            },
            queued_entry(LIVE_HEADER_SIBLING, BatchToolStatus::Pending),
        ];
        if nested {
            entries = vec![BatchToolEntry {
                status: BatchToolStatus::Running,
                ..nested_batch_entry(entries)
            }];
        }
        render_tool_content(
            None,
            Some(&ToolOutput::Batch {
                entries,
                text: String::new(),
            }),
            false,
            limits(BatchViews::new([0])).with_width(NARROW_BODY_WIDTH),
        )
    }

    /// The reported bug: a heading held to one row for as long as any sibling
    /// was still running cut the path off the call the reader was watching
    /// arrive. A call in the main conversation is read while it streams, so it
    /// wraps at every status its children pass through.
    #[test_case(BatchToolStatus::Pending, false ; "pending")]
    #[test_case(BatchToolStatus::Running, false ; "running")]
    #[test_case(BatchToolStatus::Success, false ; "success")]
    #[test_case(BatchToolStatus::Error, false ; "error")]
    #[test_case(BatchToolStatus::Running, true ; "nested_running")]
    #[test_case(BatchToolStatus::Success, true ; "nested_success")]
    fn a_batch_child_heading_wraps_to_its_whole_summary(status: BatchToolStatus, nested: bool) {
        let frame = batch_header_frame(status, nested);
        let drawn: Vec<String> = frame.lines.iter().map(line_text).collect();
        // A break puts the trunk glyph between the halves of the summary, and
        // those glyphs are the only non-ASCII on these rows, so dropping them
        // rejoins the path wherever the width happened to split it.
        let rejoined: String = drawn
            .concat()
            .chars()
            .filter(|c| c.is_ascii() && !c.is_ascii_whitespace())
            .collect();

        assert!(
            rejoined.contains(LIVE_HEADER_PATH),
            "{LIVE_HEADER_WHOLE}: {drawn:#?}"
        );
        assert!(
            !drawn.iter().any(|row| row.contains(LIVE_HEADER_PATH)),
            "{LIVE_HEADER_BROKE}: {drawn:#?}"
        );
        assert!(
            drawn.iter().any(|row| row.contains(LIVE_HEADER_SIBLING)),
            "{LIVE_HEADER_WHOLE}: {drawn:#?}"
        );
        assert_eq!(frame.lines.len(), frame.rows.len(), "{ROWS_PER_CARD_LINE}");
        let source = frame.source.as_ref().expect(CARD_RECORDS_SOURCE);
        assert!(
            source.text.contains(LIVE_HEADER_PATH),
            "{CARD_RECORDS_SOURCE}"
        );
        assert_eq!(source.rows.len(), frame.lines.len(), "{ROWS_PER_CARD_LINE}");
    }

    /// While the arguments are still arriving the child list genuinely is not
    /// known, so a roster gaining a row there is the call telling the truth
    /// about what it has read so far.
    #[test_case(1 ; "one_named")]
    #[test_case(2 ; "two_named")]
    #[test_case(3 ; "all_named")]
    fn a_roster_still_being_spelled_out_draws_the_children_named_so_far(named: usize) {
        let rows = single_level_rows(&vec![BatchToolStatus::Pending; named]);

        assert_eq!(rows.len(), named, "{ROSTER_GROWS_MSG}: {rows:?}");
    }

    /// The level that has nothing above it answers what it answered before:
    /// one row per child, in dispatch order, whatever each one has got to.
    #[test]
    fn a_single_level_roster_keeps_one_row_per_child_through_every_state() {
        let queued = single_level_rows(&[BatchToolStatus::Pending; ROSTER_CALLS.len()]);
        let mixed = single_level_rows(&[
            BatchToolStatus::Success,
            BatchToolStatus::Running,
            BatchToolStatus::Pending,
        ]);

        assert_eq!(mixed.len(), queued.len(), "{ROSTER_HELD_MSG}: {mixed:?}");
        let named: Vec<&String> = mixed
            .iter()
            .zip(ROSTER_CALLS)
            .filter(|(row, call)| row.contains(call))
            .map(|(row, _)| row)
            .collect();
        assert_eq!(
            named.len(),
            ROSTER_CALLS.len(),
            "{ROSTER_HELD_MSG}: {mixed:?}"
        );
    }

    fn shell_child(status: BatchToolStatus) -> BatchToolEntry {
        BatchToolEntry {
            status,
            ..batch_entry(SHELL_CHILD, 0)
        }
    }

    /// What the held clock reads for a running child: far enough from the
    /// measured time that a row drawing the wrong one of the two cannot pass.
    const CHILD_RAN_FOR: Duration = Duration::from_millis(1_200);
    const CHILD_LIVE_CLOCK: &str = " · 1.2s";
    const CHILD_MEASURED_MS: u64 = 10;
    const CHILD_MEASURED_CLOCK: &str = " · 10ms";
    const CHILD_TIMEOUT_MS: u64 = 120_000;
    const CHILD_CLOCK_MSG: &str = "a child row keeps the clock its standalone card would";

    fn started(index: usize) -> RenderLimits {
        RenderLimits {
            started: Arc::new(HashMap::from([(index, Instant::now())])),
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
        let _clock = FrozenClock::at(CHILD_RAN_FOR);
        let card = render_batch(&[shell_child(BatchToolStatus::Running)], false, &started(0));
        let row = line_text(&card.lines[0]);

        assert!(row.ends_with(CHILD_LIVE_CLOCK), "{CHILD_CLOCK_MSG}: {row}");
    }

    /// The subprocess's own time supersedes the wall clock the row was drawn
    /// with, exactly as a standalone card's header does.
    #[test]
    fn a_settled_shell_child_reports_the_commands_own_time() {
        let _clock = FrozenClock::at(CHILD_RAN_FOR);
        let entry = BatchToolEntry {
            output: Some(shell_child_output(CHILD_MEASURED_MS)),
            ..shell_child(BatchToolStatus::Success)
        };
        let card = render_batch(&[entry], false, &started(0));
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
        let card = render_batch(&[entry], false, &started(0));

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

    const CHILD_TIMEOUT_SECS: u64 = 120;
    const CHILD_COMMAND: &str = "cargo test";
    const CHILD_WORKDIR: &str = "crates/core";
    /// The deadline, then the directory last, set off the way a standalone
    /// card sets it off.
    const CHILD_ANNOTATION_TAIL: &str = "2m timeout · crates/core/)";
    const COMMAND_KEY: &str = "command=";
    const TIMEOUT_KEY: &str = "timeoutSec=";
    const WORKDIR_KEY: &str = "workdir=";
    const CHILD_ARGS_MSG: &str = "a child's brackets carry what its own header does not show";
    const CHILD_WORKDIR_MSG: &str = "a child names where it ran the way a standalone card does";

    /// The same filter a standalone row uses, reached through the same table,
    /// so a child cannot print the command it has already drawn. The deadline
    /// and the directory are named where a standalone card names them: once,
    /// in the annotation, rather than a second time in brackets.
    #[test]
    fn a_child_row_never_repeats_its_header_in_brackets() {
        let mut entry = batch_entry(SHELL_WIRE_CHILD, 0);
        let summary = entry.summary.clone();
        entry.raw_input = Some(serde_json::json!({
            "command": summary,
            "workdir": CHILD_WORKDIR,
            "timeoutSec": CHILD_TIMEOUT_SECS,
        }));

        let row = child_row(entry);
        for key in [COMMAND_KEY, TIMEOUT_KEY, WORKDIR_KEY] {
            assert!(!row.contains(key), "{CHILD_ARGS_MSG}: {row:?}");
        }
        assert!(
            row.contains(CHILD_ANNOTATION_TAIL),
            "{CHILD_ARGS_MSG}: {row:?}"
        );
    }

    /// Once the call lands, its result says where it really ran, so a roster
    /// restored with no session directory to resolve against still says it.
    #[test]
    fn a_settled_shell_child_names_where_it_ran() {
        let ToolOutput::Shell(output) = shell_child_output(CHILD_MEASURED_MS) else {
            unreachable!()
        };
        let entry = BatchToolEntry {
            raw_input: Some(serde_json::json!({ "command": CHILD_COMMAND })),
            output: Some(ToolOutput::Shell(ShellOutput {
                relative_workdir: CHILD_WORKDIR.into(),
                ..output
            })),
            ..shell_child(BatchToolStatus::Success)
        };
        let card = render_batch(&[entry], false, &limits(BatchViews::default()));
        let row = line_text(&card.lines[0]);

        assert!(
            row.contains(CHILD_ANNOTATION_TAIL),
            "{CHILD_WORKDIR_MSG}: {row:?}"
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

    #[test_case(false, false, false, false, false, false; "settled_shell_folds")]
    #[test_case(true, false, false, false, false, true; "streaming_shell_opens")]
    #[test_case(false, false, true, false, false, false; "always_collapsed_settled_shell_folds")]
    #[test_case(true, false, true, false, false, false; "always_collapsed_streaming_shell_folds")]
    #[test_case(true, false, false, true, false, false; "compact_streaming_shell_folds")]
    #[test_case(true, false, false, false, true, false; "ancestor_body_folds_streaming_shell")]
    #[test_case(false, true, false, false, false, true; "explicit_open_settled_shell")]
    #[test_case(false, true, true, false, false, true; "explicit_open_overrides_always_collapsed")]
    #[test_case(true, true, true, false, false, true; "explicit_open_overrides_always_collapsed_stream")]
    #[test_case(false, true, false, true, false, true; "explicit_open_overrides_compact")]
    #[test_case(false, true, false, false, true, true; "explicit_open_overrides_ancestor_body")]
    fn supplied_child_windows_respect_visibility(
        streaming: bool,
        open: bool,
        always_collapsed: bool,
        compact: bool,
        body_taken: bool,
        visible: bool,
    ) {
        const VISIBILITY_MSG: &str = "a supplied window sizes a visible child but never opens it";
        const WINDOW_MSG: &str = "an opened child retains its supplied window and reading position";
        const WINDOW: ScrollWindow = ScrollWindow {
            height: NESTED_WINDOW_LINES as usize,
            offset: 1,
            follow: false,
        };
        let mut entry = BatchToolEntry {
            effect: ToolEffect::Mutating,
            ..batch_entry(SHELL_CHILD, NESTED_BODY_LINES)
        };
        if streaming {
            entry.status = BatchToolStatus::Running;
            entry.output = None;
        }
        let limits = RenderLimits {
            body_taken,
            live: Arc::new(HashMap::from([(
                0,
                [CHILD_BODY; NESTED_BODY_LINES].join("\n"),
            )])),
            ..limits(BatchViews::new(open.then_some(0))).with_policy(
                CardPolicy {
                    compact,
                    always_collapsed: if always_collapsed {
                        Arc::from([SHELL_CHILD.to_owned()])
                    } else {
                        Arc::default()
                    },
                    scroll_card_lines: NESTED_WINDOW_LINES * 2,
                    ..CardPolicy::default()
                },
                Arc::new(HashMap::from([(0, WINDOW)])),
            )
        };
        let child = limits.child(0, &entry);
        assert_eq!(child.is_some(), visible, "{VISIBILITY_MSG}");
        if let Some(child) = child {
            assert_eq!(child.scroll, Some(WINDOW), "{WINDOW_MSG}");
            assert_eq!(child.budget, WINDOW.height, "{WINDOW_MSG}");
            assert!(child.body_taken, "{ONE_BODY_MSG}");
            assert!(child.child_scroll.is_empty(), "{WINDOW_MSG}");
            assert_eq!(child.views, BatchViews::default());
            assert!(
                child
                    .child(0, &dispatch_entry(NESTED_CHILD, NESTED_BODY_LINES))
                    .is_none(),
                "{ONE_BODY_MSG}"
            );
        }
        let card = render_batch(&[entry], false, &limits);
        assert_eq!(
            body_count(&card.lines),
            if visible { WINDOW.height } else { 0 },
            "{VISIBILITY_MSG}"
        );
        assert_eq!(card.spans.len(), usize::from(visible), "{WINDOW_MSG}");
    }

    #[test_case("queued", false, true; "queued_receipt_opens")]
    #[test_case("running", false, true; "active_receipt_opens")]
    #[test_case("cancelling", false, true; "cancelling_receipt_opens")]
    #[test_case("succeeded", false, false; "terminal_receipt_folds")]
    #[test_case("failed", false, false; "failed_receipt_folds")]
    #[test_case("cancelled", false, false; "cancelled_receipt_folds")]
    #[test_case("running", true, false; "always_collapsed_hides_active_receipt")]
    #[test_case("succeeded", true, false; "always_collapsed_hides_terminal_receipt")]
    fn shell_receipt_visibility_follows_task_state(
        state: &str,
        always_collapsed: bool,
        visible: bool,
    ) {
        const RECEIPT_MSG: &str = "admission succeeds before the shell receipt becomes terminal";
        const OPEN_MSG: &str = "explicit opening overrides automatic receipt visibility";
        let receipt = serde_json::from_value(serde_json::json!({
            "kind": "shell", "task_id": "shell-task", "invocation_id": "invocation",
            "call_id": "batch:0", "root_call_id": "batch", "label": "Print", "state": state,
            "mode": "build", "background": true, "generation": 1, "created_at": 1, "updated_at": 2,
        }))
        .unwrap();
        let entry = BatchToolEntry {
            effect: ToolEffect::Mutating,
            output: Some(ToolOutput::Tasks(vec![receipt])),
            ..batch_entry(SHELL_CHILD, 0)
        };
        let mut limits = scrolling_limits(BatchViews::default());
        if always_collapsed {
            limits.policy.always_collapsed = Arc::from([SHELL_CHILD.to_owned()]);
        }
        assert_eq!(limits.child(0, &entry).is_some(), visible, "{RECEIPT_MSG}");
        limits.policy.compact = true;
        assert!(limits.child(0, &entry).is_none(), "{RECEIPT_MSG}");
        limits.policy.compact = false;
        limits.body_taken = true;
        assert!(limits.child(0, &entry).is_none(), "{ONE_BODY_MSG}");
        limits.policy.compact = true;
        limits.views = BatchViews::new([0]);
        assert!(limits.child(0, &entry).is_some(), "{OPEN_MSG}");
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
            unique_targets(&rows).contains(&RowTarget::Item(0)),
            "and the row it says it on has to take the click"
        );
    }

    #[test]
    fn asking_for_a_resting_child_draws_it_whole() {
        let (lines, rows) = write_batch(WRITE_BUDGET * 2, BatchViews::new([0]));

        assert_eq!(body_count(&lines), WRITE_BUDGET * 2, "{CHANGED_MSG}");
        assert!(
            unique_targets(&rows).contains(&RowTarget::Item(0)),
            "an opened child stays a control, or it could not be put back"
        );
    }

    /// A dispatched child is traced back to its subagent through the rows it
    /// owns, so every row of one keeps naming it however it was drawn.
    #[test]
    fn every_row_of_a_resting_child_still_names_it() {
        let (lines, rows) = write_batch(2, BatchViews::default());

        assert_eq!(lines.len(), rows.len(), "the rows stay parallel");
        assert_eq!(unique_targets(&rows), vec![RowTarget::Item(0)]);
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
            question: LONG.to_owned(),
            options: Vec::from([QuestionOption {
                label: LONG.to_owned(),
                description: LONG.to_owned(),
            }]),
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

    fn skill_output() -> ToolOutput {
        ToolOutput::Skill(SkillOutput {
            location: LONG.to_owned(),
            body: format!("# {LONG}\n\n{LONG}"),
        })
    }

    /// A child whose body is structured rather than prose. Prose is broken to
    /// the child's own width before it is indented, so it is the structured
    /// renderers that reach the card's break still carrying a full-width row.
    fn todo_entry() -> BatchToolEntry {
        BatchToolEntry {
            output: Some(todo_output()),
            ..batch_entry(caudra_agent::tools::TODOWRITE_TOOL_NAME, 0)
        }
    }

    /// Two children, so one is followed by a sibling and one is not, with
    /// bodies too long for the card. The deepest gutters a card ever draws.
    fn batch_output() -> ToolOutput {
        ToolOutput::Batch {
            entries: Vec::from([todo_entry(), todo_entry()]),
            text: LONG.to_owned(),
        }
    }

    /// The whole point of the invariant: one case per output a card can draw,
    /// so a renderer added later has to join the list rather than quietly
    /// reintroducing the bug this fixes.
    #[test_case(batch_output() ; "batch")]
    #[test_case(todo_output() ; "todo_list")]
    #[test_case(answers_output() ; "answers")]
    #[test_case(grep_output() ; "grep_result")]
    #[test_case(environment_output() ; "environment")]
    #[test_case(read_code_output() ; "read_code")]
    #[test_case(code_graph_output() ; "code_graph")]
    #[test_case(skill_output() ; "skill")]
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

    const TRUNK_UNBROKEN: &str = "a child with a sibling below it carries the trunk down every row \
        it owns, wrapped rows included, or the tree is cut in two at the first row too long for \
        the card";
    const TRUNK_ENDS: &str = "nothing follows the last child, so its rows carry no trunk";

    /// The reported bug: a break put spaces where the trunk was, so the tree
    /// came apart at exactly the rows that needed it most.
    #[test]
    fn a_wrapped_row_carries_the_trunk_down_the_side_of_its_child() {
        let mut limits = limits(BatchViews::new([0, 1]));
        limits.width = INVARIANT_WIDTH;

        let content = render_tool_content(None, Some(&batch_output()), false, limits);
        let rows: Vec<String> = content.lines.iter().map(line_text).collect();
        let last = rows
            .iter()
            .position(|row| row.starts_with(TREE_LAST))
            .expect(TRUNK_ENDS);

        assert!(last > 1, "{TRUNK_UNBROKEN}: {rows:#?}");
        for row in rows.iter().take(last).filter(|row| !row.trim().is_empty()) {
            let opens = row.starts_with(TREE_BRANCH) || row.starts_with(TREE_TRUNK.trim_end());
            assert!(opens, "{TRUNK_UNBROKEN}: {row:?}");
        }
        for row in rows.iter().skip(last + 1) {
            assert!(
                !row.starts_with(TREE_TRUNK.trim_end()),
                "{TRUNK_ENDS}: {row:?}"
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

    /// One word to a row at [`ROWS_OF_WIDTH`]: the lines take three rows, one
    /// and two.
    const ROWS_OF_LINES: [&str; 3] = ["alpha bravo charlie", "delta", "echo foxtrot"];
    const ROWS_OF_WIDTH: u16 = 8;

    #[test]
    fn lines_map_onto_the_rows_they_start_on_and_past_the_end_onto_the_total() {
        let lines = ROWS_OF_LINES.into_iter().map(Line::from).collect();
        let wrapped = WrappedRows::new(lines, 0, ROWS_OF_WIDTH);

        assert_eq!(wrapped.rows_of(&[0, 1, 2, 3, 9]), [0, 3, 4, 6, 6]);
    }
}
