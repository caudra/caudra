use std::collections::HashMap;
use std::sync::Arc;

use crate::highlight::{fallback_span, highlight_line};
use crate::markdown::{expand_notice, should_truncate, truncation_notice};
use crate::theme;

use super::tool_display::{batch_sigil_style, compact_args_for, compact_sigil_label, header_spans};
use caudra_agent::diff::{DiffLine, DiffSpan, compute_hunks};
use caudra_agent::types::Answer;
use caudra_agent::types::{TodoItem, TodoStatus};
use caudra_agent::{
    BatchToolEntry, BatchToolStatus, GrepFileEntry, INDEX_TRUNCATED, IndexDirectoryEntryKind,
    IndexLine, IndexLineSemantic, IndexOutput, IndexSourceRange, InstructionBlock, PatchedFile,
    SearchCap, ToolInput, ToolOutput,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::parsing::SyntaxReference;
use syntect::util::LinesWithEndings;

pub(crate) const MAX_INSTRUCTION_LINES: usize = 15;
const BATCH_CHILD_INDENT: &str = "  ";
const ANSWER_MARK: &str = "  \u{2713} ";
const ANSWER_INDENT: &str = "    ";
const NO_ANSWER: &str = "(no answer)";
/// Says a child is folded, so a row with nothing under it is not mistaken for
/// one whose body was hidden.
const BATCH_FOLDED_MARK: &str = " \u{2026}";
const GREP_COUNT_SEP: &str = " \u{b7} ";
const GREP_SUMMARY_INDENT: &str = "  ";

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

fn render_code(
    mut hl: Option<caudra_highlight::Highlighter>,
    start_line: usize,
    code_lines: &[String],
    total_count: usize,
    max_lines: usize,
) -> (Vec<Line<'static>>, bool) {
    let capped = code_lines.len().min(max_lines);
    let hidden = total_count.saturating_sub(capped);
    let has_truncation = should_truncate(hidden);
    let display_count = if has_truncation {
        capped
    } else {
        code_lines.len()
    };
    let max_nr = start_line + display_count.saturating_sub(1);
    let w = nr_width(max_nr);

    let mut lines: Vec<Line<'static>> = code_lines
        .iter()
        .take(display_count)
        .enumerate()
        .map(|(i, text)| {
            let nr = start_line + i;
            let mut spans = vec![gutter(&format!("{nr:>w$}"))];
            match &mut hl {
                Some(h) => spans.extend(highlight_spans(h, text)),
                None => spans.push(fallback_span(text)),
            }
            Line::from(spans)
        })
        .collect();

    if has_truncation {
        lines.push(truncation_line(hidden));
    }
    (lines, has_truncation)
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

fn render_diff(
    syntax: Option<&'static SyntaxReference>,
    before: &str,
    after: &str,
) -> Vec<Line<'static>> {
    let hunks = compute_hunks(before, after);
    let Some(last) = hunks.last() else {
        return Vec::new();
    };
    let numbered = last
        .lines
        .iter()
        .filter(|l| !matches!(l, DiffLine::Added(_)))
        .count();
    let w = nr_width(last.before_start + numbered.saturating_sub(1));

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

        let mut line_nr = hunk.before_start;
        for dl in &hunk.lines {
            lines.push(render_hunk_line(dl, walkers.as_mut(), &mut line_nr, w));
        }
    }

    lines
}

fn numbered_gutter(line_nr: &mut usize, w: usize) -> Span<'static> {
    let span = gutter(&format!("{line_nr:>w$}"));
    *line_nr += 1;
    span
}

/// Unchanged lines step both walkers but take spans from `after`.
/// Removed/added lines only step their own side.
fn render_hunk_line(
    dl: &DiffLine,
    walkers: Option<&mut (FileWalker<'_>, FileWalker<'_>)>,
    line_nr: &mut usize,
    w: usize,
) -> Line<'static> {
    let theme = theme::current();
    match dl {
        DiffLine::Unchanged(t) => {
            let after_spans = walkers.and_then(|(before, after)| {
                before.skip();
                after.highlight_next()
            });
            let mut spans = vec![numbered_gutter(line_nr, w), Span::raw("  ")];
            spans.extend(syntax_to_spans(after_spans, t));
            Line::from(spans)
        }
        DiffLine::Removed(ds) => {
            let before_spans = walkers.and_then(|(before, _)| before.highlight_next());
            let mut spans = vec![numbered_gutter(line_nr, w)];
            spans.extend(diff_change_spans(
                "- ",
                ds,
                before_spans,
                theme.diff_old,
                theme.diff_old_emphasis,
            ));
            Line::from(spans)
        }
        DiffLine::Added(ds) => {
            let after_spans = walkers.and_then(|(_, after)| after.highlight_next());
            let mut spans = vec![gutter(&" ".repeat(w))];
            spans.extend(diff_change_spans(
                "+ ",
                ds,
                after_spans,
                theme.diff_new,
                theme.diff_new_emphasis,
            ));
            Line::from(spans)
        }
    }
}

fn syntax_to_spans(syntax: Option<Vec<Span<'static>>>, text: &str) -> Vec<Span<'static>> {
    match syntax {
        Some(s) => s,
        None => vec![fallback_span(text)],
    }
}

fn diff_change_spans(
    prefix: &'static str,
    ds: &[DiffSpan],
    syntax: Option<Vec<Span<'static>>>,
    base: Style,
    emph: Style,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        prefix,
        base.patch(theme::current().code_block),
    )];
    match syntax {
        Some(syn) => spans.extend(merge_syntax_with_diff(&syn, ds, base, emph)),
        None => {
            let full: String = ds.iter().map(|s| s.text.as_str()).collect();
            spans.push(Span::styled(
                caudra_highlight::normalize_text(&full),
                base.patch(theme::current().code_block),
            ));
        }
    }
    spans
}

/// The `-a` of a `@@ -a,b +c,d @@` header, which is where the hunk's numbering
/// restarts. `None` for any line that is not a hunk header.
fn hunk_start(line: &str) -> Option<usize> {
    line.strip_prefix("@@ -")?
        .split(&[',', ' '][..])
        .next()?
        .parse()
        .ok()
}

/// Widest line number the patch will print, so every gutter lines up.
fn patch_nr_width(patch: &str) -> usize {
    let mut width = 1;
    let mut nr = 0;
    for line in patch.lines() {
        match hunk_start(line) {
            Some(start) => nr = start,
            None if !line.starts_with('+') => {
                width = width.max(nr_width(nr));
                nr += 1;
            }
            None => {}
        }
    }
    width
}

/// A unified diff drawn the way an edit's diff is drawn: real line numbers
/// down the left, removed and added lines in the diff colours. Syntax
/// highlighting is left out on purpose, because a hunk carries only its own
/// context and a highlighter fed that much guesses wrong more than it helps.
fn render_unified_patch(patch: &str) -> Vec<Line<'static>> {
    let theme = theme::current();
    let width = patch_nr_width(patch);
    let mut lines = Vec::new();
    let mut line_nr = 0;
    let mut in_hunk = false;
    for raw in patch.lines() {
        if let Some(start) = hunk_start(raw) {
            if in_hunk {
                lines.push(gap_ellipsis());
            }
            (line_nr, in_hunk) = (start, true);
            continue;
        }
        // File headers only precede the first hunk, so a later line starting
        // the same way is content and keeps its numbering.
        if !in_hunk {
            continue;
        }
        let (prefix, style, text) = match raw.split_at_checked(1) {
            Some(("-", rest)) => ("- ", theme.diff_old, rest),
            Some(("+", rest)) => ("+ ", theme.diff_new, rest),
            Some((" ", rest)) => ("  ", theme.code_block, rest),
            _ => ("  ", theme.code_block, raw),
        };
        let numbered = prefix != "+ ";
        let mut spans = vec![if numbered {
            gutter(&format!("{line_nr:>width$}"))
        } else {
            gutter(&" ".repeat(width))
        }];
        spans.push(Span::styled(prefix, style.patch(theme.code_block)));
        spans.push(Span::styled(
            caudra_highlight::normalize_text(text),
            style.patch(theme.code_block),
        ));
        lines.push(Line::from(spans));
        if numbered {
            line_nr += 1;
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
) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>) {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut previous_has_body = false;
    for (index, entry) in entries.iter().enumerate() {
        let open = limits.child_open(index);
        // Resolved before the summary row so the separator below knows whether
        // this child is a list entry or a block.
        let body = open.then(|| child_body(entry, highlight, &limits.for_child()));
        let has_body = body.as_ref().is_some_and(|body| !body.is_empty());
        if !lines.is_empty() && (previous_has_body || has_body) {
            lines.push(Line::default());
            rows.push(None);
        }
        previous_has_body = has_body;
        let (sigil, label) = compact_sigil_label(&entry.tool, entry.status.into());
        let mut spans = vec![
            Span::styled(
                format!("{sigil} "),
                batch_sigil_style(entry.status, entry.output.as_ref()),
            ),
            Span::styled(format!("{label} "), t.tool_prefix),
        ];
        spans.extend(header_spans(
            &entry.tool,
            &entry.summary,
            Style::default(),
            entry.raw_input.as_ref(),
        ));
        if let Some(args) = compact_args_for(&entry.tool, &entry.summary, entry.raw_input.as_ref())
        {
            spans.push(Span::styled(args, t.tool_dim));
        }
        if let Some(annotation) = child_annotation(entry) {
            spans.push(Span::styled(format!(" ({annotation})"), t.tool_annotation));
        }
        if !open {
            spans.push(Span::styled(BATCH_FOLDED_MARK, t.tool_dim));
        }
        lines.push(Line::from(spans));
        rows.push(Some(RowTarget(index)));
        if let Some(body) = body {
            rows.resize(rows.len() + body.len(), Some(RowTarget(index)));
            lines.extend(indent_all(body));
        }
    }
    (lines, rows)
}

/// What a child row reports after its arguments. `batch` carries only the
/// annotation the dispatch returned, which is `None` for every tool that lets
/// its output speak instead, so a child lost the count its standalone row
/// shows: a grep of thirty files named no matches at all. A failure is left
/// out because its output is the error text, which the body already draws in
/// full and which reduces to a line count here.
fn child_annotation(entry: &BatchToolEntry) -> Option<String> {
    if let Some(annotation) = &entry.annotation {
        return Some(annotation.clone());
    }
    (entry.status == BatchToolStatus::Success)
        .then(|| entry.output.as_ref().and_then(ToolOutput::annotation))
        .flatten()
}

/// A child's own rendering, structured where the tool produced structure and
/// its text otherwise. Errors read as plain text: a failed call has no
/// structured result to draw. An opened child draws whole, so nothing here
/// truncates.
fn child_body(
    entry: &BatchToolEntry,
    highlight: bool,
    limits: &RenderLimits,
) -> Vec<Line<'static>> {
    let output = entry.output.as_ref();
    if entry.status == BatchToolStatus::Error {
        return text_lines(output.map_or(String::new(), ToolOutput::as_text));
    }
    match output {
        Some(ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text)) => {
            text_lines(text.text.clone())
        }
        Some(ToolOutput::Shell(shell)) => text_lines(shell.raw_text()),
        other => render_tool_content(entry.input.as_ref(), other, highlight, limits.clone()).lines,
    }
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
fn render_patch(files: &[PatchedFile]) -> Vec<Line<'static>> {
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
        lines.extend(render_unified_patch(&file.patch));
    }
    lines
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
        let (rendered, was_truncated) = render_code(hl, 1, &code_lines, total, remaining);
        used += rendered.len();
        truncated |= was_truncated;
        lines.extend(rendered);
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

/// The batch children the reader has opened, by their index in the roster.
/// Children start folded, so a card nobody has touched reads as the list of
/// what it ran rather than every result at once.
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

#[derive(Clone, Default)]
pub struct RenderLimits {
    pub budget: usize,
    pub views: BatchViews,
}

impl RenderLimits {
    pub fn new(full: bool, budget: usize, views: BatchViews) -> Self {
        Self {
            budget: if full { usize::MAX } else { budget },
            views,
        }
    }

    pub fn is_expanded(&self) -> bool {
        self.budget == usize::MAX
    }

    /// A child the reader has opened draws whole. Opening the parent says
    /// nothing about its children: the card's own body is the list of what
    /// ran, and each child answers for itself.
    fn child_open(&self, index: usize) -> bool {
        self.views.is_open(index)
    }

    /// A child renders on its own terms. The views name this card's children,
    /// so carrying them inward would fold a nested batch by the wrong roster.
    fn for_child(&self) -> Self {
        Self {
            budget: usize::MAX,
            views: BatchViews::default(),
        }
    }
}

/// The child views of every card that has any, by parent tool id.
pub type BatchViewMap = HashMap<String, BatchViews>;

/// The batch child a body line belongs to, by its roster index, so a click can
/// name a row after the async highlight has replaced the spans under it. A
/// child is folded or whole, so its summary row and its body are one control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowTarget(pub usize);

pub struct ToolContent {
    pub lines: Vec<Line<'static>>,
    /// Parallel to `lines`. Both render paths build it the same way, so the
    /// highlighted lines carry the same rows as the ones they replace.
    pub rows: Vec<Option<RowTarget>>,
    pub truncation: bool,
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
        let (code_result, trunc) = render_code(hl, 1, &code_lines, total, limits.budget);
        truncation = trunc;
        lines.extend(code_result);
    }
    let (output_lines, output_trunc) = match output {
        Some(ToolOutput::ReadCode {
            path,
            start_line,
            lines: code_lines,
            ..
        }) => render_code(
            highlight.then(|| caudra_highlight::Highlighter::for_path(path)),
            *start_line,
            code_lines,
            code_lines.len(),
            limits.budget,
        ),
        Some(ToolOutput::WriteCode {
            path,
            lines: code_lines,
            ..
        }) => render_code(
            highlight.then(|| caudra_highlight::Highlighter::for_path(path)),
            1,
            code_lines,
            code_lines.len(),
            limits.budget,
        ),
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
            ),
            false,
        ),
        Some(ToolOutput::Patch { files }) => (render_patch(files), false),
        Some(ToolOutput::GrepResult { entries, capped }) => {
            render_grep_results(entries, capped.as_ref(), limits.budget, highlight)
        }
        Some(ToolOutput::Index(IndexOutput::File {
            language, lines, ..
        })) => render_index_file(language, lines, limits.budget, highlight),
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
        // Each child owns how much of itself it shows, so the card reports no
        // truncation of its own: there is no one thing for it to open.
        Some(ToolOutput::Batch { entries, .. }) if !entries.is_empty() => {
            let (batch_lines, rows) = render_batch(entries, highlight, &limits);
            output_rows = rows;
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
    lines.extend(output_lines);
    ToolContent {
        lines,
        rows,
        truncation,
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
        let (result, _) = render_code(
            Some(caudra_highlight::Highlighter::for_path("test.rs")),
            1,
            &code_lines,
            total,
            READ_MAX_LINES,
        );
        assert_eq!(result.len(), expected);
    }

    const PATCH: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -8,3 +8,4 @@\n context\n-gone\n+added\n+also added\n";
    const NUMBERED_MSG: &str = "context and removed lines carry their real file line number";
    const BLANK_GUTTER_MSG: &str = "an added line has no line number on the before side";
    const HUNK_GAP_MSG: &str = "a jump between hunks must be marked, not silently closed";
    const HEADING_MSG: &str = "each file names itself and its size";

    fn patch_text(files: &[PatchedFile]) -> Vec<String> {
        render_patch(files).iter().map(line_text).collect()
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
            rendered.contains(&"8   context".to_owned()),
            "{NUMBERED_MSG}: {rendered:?}"
        );
        assert!(
            rendered.contains(&"9 - gone".to_owned()),
            "{NUMBERED_MSG}: {rendered:?}"
        );
        assert!(
            rendered.contains(&"  + added".to_owned()),
            "{BLANK_GUTTER_MSG}: {rendered:?}"
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
    /// What `batch` itself resolves to. A child never reads it: opened, it
    /// draws whole.
    const PARENT_BUDGET: usize = 3;

    fn batch_entry(tool: &str, body_lines: usize) -> BatchToolEntry {
        let text = (0..body_lines)
            .map(|i| format!("{CHILD_BODY} {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        BatchToolEntry {
            tool: tool.into(),
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

    fn batch_of(
        sizes: [usize; 2],
        views: BatchViews,
    ) -> (Vec<Line<'static>>, Vec<Option<RowTarget>>) {
        let entries = [batch_entry("read", sizes[0]), batch_entry("grep", sizes[1])];
        render_batch(
            &entries,
            false,
            &RenderLimits::new(false, PARENT_BUDGET, views),
        )
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
        let limits = RenderLimits::new(false, PARENT_BUDGET, BatchViews::new([0]));
        assert_eq!(limits.for_child().views, BatchViews::default());
    }

    /// A child used to draw within its own tool's budget, which made the card
    /// a third state between folded and whole. Opening one now answers the
    /// question the reader asked.
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
        let limits = RenderLimits::new(false, PARENT_BUDGET, BatchViews::default());
        line_text(&render_batch(&[entry], false, &limits).0[0])
    }

    fn child_sigil(entry: BatchToolEntry) -> Span<'static> {
        let limits = RenderLimits::new(false, PARENT_BUDGET, BatchViews::default());
        render_batch(&[entry], false, &limits).0[0].spans[0].clone()
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
        let (lines, rows) = render_batch(
            &entries,
            false,
            &RenderLimits::new(false, PARENT_BUDGET, BatchViews::new([1])),
        );

        assert_eq!(blank_rows(&lines), vec![1, 5], "{BATCH_BODY_AIR_MSG}");
        assert_eq!(lines.len(), rows.len(), "the rows stay parallel");
    }

    /// The card's own body is the list of what it ran, so opening it says
    /// nothing about the children. Otherwise one click on a card of ten greps
    /// would write ten whole greps into the transcript.
    #[test]
    fn an_opened_card_leaves_its_children_folded() {
        let whole = 6;
        let limits = RenderLimits::new(true, PARENT_BUDGET, BatchViews::default());
        let entries = [batch_entry("read", whole), batch_entry("grep", whole)];
        let (lines, _) = render_batch(&entries, false, &limits);
        assert_eq!(body_count(&lines), 0);
    }
}
